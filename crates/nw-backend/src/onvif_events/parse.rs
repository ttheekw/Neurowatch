//! Reading ONVIF event-service replies. Nothing here trusts a URL the
//! device sends: every address is pinned to the device's own address.

use std::net::Ipv4Addr;

use crate::discovery::onvif::{pin_http_url, Addressing};

fn local<'a>(node: &roxmltree::Node<'a, '_>) -> &'a str {
    node.tag_name().name()
}

fn child<'a, 'i>(node: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    node.children().find(|n| n.is_element() && local(n) == name)
}

fn text(node: roxmltree::Node) -> Option<String> {
    node.text()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// What `GetCapabilities` says about the device's event and PTZ services.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Services {
    pub events_url: Option<String>,
    pub pull_point: bool,
    pub ptz_url: Option<String>,
}

pub fn services(reply: &str, device: Ipv4Addr) -> Option<Services> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    let capabilities = doc
        .descendants()
        .find(|n| n.is_element() && local(n) == "Capabilities")?;
    let service = |name: &str| {
        capabilities
            .children()
            .find(|n| n.is_element() && local(n) == name)
    };
    let events = service("Events");
    Some(Services {
        events_url: events
            .and_then(|e| child(e, "XAddr"))
            .and_then(text)
            .and_then(|url| pin_http_url(&url, device)),
        pull_point: events
            .and_then(|e| child(e, "WSPullPointSupport"))
            .and_then(text)
            .is_some_and(|t| t.eq_ignore_ascii_case("true") || t == "1"),
        ptz_url: service("PTZ")
            .and_then(|p| child(p, "XAddr"))
            .and_then(text)
            .and_then(|url| pin_http_url(&url, device)),
    })
}

/// A pull-point subscription: where to pull, and how to address it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub addressing: Addressing,
}

/// The `CreatePullPointSubscription` reply. Reference parameters are written
/// again with their own namespace declarations (a copy of the raw XML would
/// lose prefixes declared further up).
pub fn subscription(reply: &str, device: Ipv4Addr) -> Option<Subscription> {
    let doc = roxmltree::Document::parse(reply).ok()?;
    let reference = doc
        .descendants()
        .find(|n| n.is_element() && local(n) == "SubscriptionReference")?;
    let address = child(reference, "Address").and_then(text)?;
    let address = pin_http_url(&address, device)?;
    let mut parameters = String::new();
    if let Some(params) = child(reference, "ReferenceParameters") {
        for element in params.children().filter(|n| n.is_element()) {
            let name = local(&element);
            let value = xml_escape(element.text().unwrap_or("").trim());
            match element.tag_name().namespace() {
                Some(ns) => parameters.push_str(&format!(
                    r#"<rp:{name} xmlns:rp="{}" xmlns:wsa="http://www.w3.org/2005/08/addressing" wsa:IsReferenceParameter="true">{value}</rp:{name}>"#,
                    xml_escape(ns)
                )),
                None => parameters.push_str(&format!("<{name}>{value}</{name}>")),
            }
        }
    }
    Some(Subscription {
        addressing: Addressing {
            to: address,
            reference_parameters: parameters,
        },
    })
}

/// How a property event changed (`PropertyOperation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operation {
    Initialized,
    Changed,
    Deleted,
    /// Not a property: something that happened once.
    None,
}

/// One `NotificationMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// Without namespace prefixes, e.g. `RuleEngine/CellMotionDetector/Motion`.
    pub topic: String,
    pub operation: Operation,
    pub source: Vec<(String, String)>,
    pub data: Vec<(String, String)>,
}

/// Longest topic and item text kept from a device.
const MAX_TEXT: usize = 200;
/// Most notifications read from one pull.
const MAX_NOTIFICATIONS: usize = 256;

fn bounded(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT)
        .collect()
}

/// `tns1:RuleEngine/CellMotionDetector/Motion` → `RuleEngine/CellMotionDetector/Motion`.
pub fn normalize_topic(topic: &str) -> String {
    bounded(
        &topic
            .trim()
            .split('/')
            .map(|part| part.rsplit_once(':').map_or(part, |(_, name)| name))
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

fn simple_items(message: roxmltree::Node, group: &str) -> Vec<(String, String)> {
    child(message, group)
        .map(|group| {
            group
                .descendants()
                .filter(|n| n.is_element() && local(n) == "SimpleItem")
                .filter_map(|item| {
                    Some((
                        bounded(item.attribute("Name")?),
                        bounded(item.attribute("Value").unwrap_or("")),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn notifications(reply: &str) -> Result<Vec<Notification>, String> {
    let doc = roxmltree::Document::parse(reply).map_err(|e| e.to_string())?;
    Ok(doc
        .descendants()
        .filter(|n| n.is_element() && local(n) == "NotificationMessage")
        .filter_map(|notification| {
            let topic = child(notification, "Topic").and_then(text)?;
            // wsnt:Message wraps the ONVIF tt:Message.
            let message = child(notification, "Message")?;
            let message = child(message, "Message").unwrap_or(message);
            let operation = match message.attribute("PropertyOperation") {
                Some("Initialized") => Operation::Initialized,
                Some("Changed") => Operation::Changed,
                Some("Deleted") => Operation::Deleted,
                _ => Operation::None,
            };
            Some(Notification {
                topic: normalize_topic(&topic),
                operation,
                source: simple_items(message, "Source"),
                data: simple_items(message, "Data"),
            })
        })
        .take(MAX_NOTIFICATIONS)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 64);

    #[test]
    fn capabilities_give_pinned_event_and_ptz_urls() {
        let reply = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tds="http://www.onvif.org/ver10/device/wsdl"><s:Body><tds:GetCapabilitiesResponse><tds:Capabilities>
            <tt:Device><tt:XAddr>http://192.168.1.64/onvif/device_service</tt:XAddr></tt:Device>
            <tt:Events><tt:XAddr>http://10.0.0.9:8080/onvif/Events</tt:XAddr><tt:WSSubscriptionPolicySupport>true</tt:WSSubscriptionPolicySupport><tt:WSPullPointSupport>true</tt:WSPullPointSupport></tt:Events>
            <tt:PTZ><tt:XAddr>http://camera.local/onvif/PTZ</tt:XAddr></tt:PTZ>
            </tds:Capabilities></tds:GetCapabilitiesResponse></s:Body></s:Envelope>"#;
        let services = services(reply, DEVICE).unwrap();
        assert_eq!(
            services.events_url.as_deref(),
            Some("http://192.168.1.64:8080/onvif/Events")
        );
        assert!(services.pull_point);
        assert_eq!(
            services.ptz_url.as_deref(),
            Some("http://192.168.1.64/onvif/PTZ")
        );
    }

    #[test]
    fn a_subscription_keeps_its_reference_parameters_with_namespaces() {
        let reply = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsa="http://www.w3.org/2005/08/addressing" xmlns:tev="http://www.onvif.org/ver10/events/wsdl" xmlns:dom0="http://www.axis.com/2009/event"><s:Body><tev:CreatePullPointSubscriptionResponse>
            <tev:SubscriptionReference><wsa:Address>http://0.0.0.0/onvif/services?sub=7</wsa:Address>
            <wsa:ReferenceParameters><dom0:SubscriptionId>7&amp;x</dom0:SubscriptionId></wsa:ReferenceParameters></tev:SubscriptionReference>
            </tev:CreatePullPointSubscriptionResponse></s:Body></s:Envelope>"#;
        let subscription = subscription(reply, DEVICE).unwrap();
        assert_eq!(
            subscription.addressing.to,
            "http://192.168.1.64/onvif/services?sub=7"
        );
        let params = &subscription.addressing.reference_parameters;
        assert!(
            params.contains(r#"xmlns:rp="http://www.axis.com/2009/event""#),
            "{params}"
        );
        assert!(params.contains(">7&amp;x</rp:SubscriptionId>"), "{params}");
    }

    #[test]
    fn notifications_carry_topic_operation_and_items() {
        let reply = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:wsnt="http://docs.oasis-open.org/wsn/b-2" xmlns:tt="http://www.onvif.org/ver10/schema" xmlns:tns1="http://www.onvif.org/ver10/topics" xmlns:tev="http://www.onvif.org/ver10/events/wsdl"><s:Body><tev:PullMessagesResponse>
            <tev:CurrentTime>2026-10-13T10:00:00Z</tev:CurrentTime>
            <wsnt:NotificationMessage><wsnt:Topic Dialect="http://www.onvif.org/ver10/tev/topicExpression/ConcreteSet">tns1:RuleEngine/CellMotionDetector/Motion</wsnt:Topic>
              <wsnt:Message><tt:Message UtcTime="2026-10-13T10:00:00Z" PropertyOperation="Changed">
                <tt:Source><tt:SimpleItem Name="VideoSourceConfigurationToken" Value="V1"/><tt:SimpleItem Name="Rule" Value="MyMotion"/></tt:Source>
                <tt:Data><tt:SimpleItem Name="IsMotion" Value="true"/></tt:Data>
              </tt:Message></wsnt:Message></wsnt:NotificationMessage>
            <wsnt:NotificationMessage><wsnt:Topic>tns1:RuleEngine/LineDetector/Crossed</wsnt:Topic>
              <wsnt:Message><tt:Message UtcTime="2026-10-13T10:00:01Z"><tt:Data><tt:SimpleItem Name="ObjectId" Value="3"/></tt:Data></tt:Message></wsnt:Message></wsnt:NotificationMessage>
            </tev:PullMessagesResponse></s:Body></s:Envelope>"#;
        let list = notifications(reply).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].topic, "RuleEngine/CellMotionDetector/Motion");
        assert_eq!(list[0].operation, Operation::Changed);
        assert_eq!(list[0].source[1], ("Rule".into(), "MyMotion".into()));
        assert_eq!(list[0].data, [("IsMotion".into(), "true".into())]);
        assert_eq!(list[1].operation, Operation::None);
    }

    #[test]
    fn topics_lose_every_prefix() {
        assert_eq!(
            normalize_topic(" tns1:VideoSource/tnsaxis:MotionAlarm "),
            "VideoSource/MotionAlarm"
        );
    }
}
