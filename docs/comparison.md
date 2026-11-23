# NeuroWatch compared with other camera recorders

Choosing software for security cameras usually comes down to a few questions: does it run on the computer you have, do you want a screen of cameras or a web page, how do you watch from outside, and how much object detection do you need? This page puts NeuroWatch beside the recorders people most often compare it with (Frigate, Shinobi, Bluecherry, ZoneMinder, Agent DVR, Viseron, Scrypted and Blue Iris), and says plainly where each one is the better fit.

The same comparison is on the website: [neurowatch.app/compare](https://neurowatch.app/compare).

> Checked against each project's public documentation on **14 November 2026**. Projects change quickly: if something here is out of date, please [open an issue](https://github.com/ttheekw/Neurowatch/issues) and it will be corrected.

> This comparison is the NeuroWatch team's own reading of each project's public documentation, written in good faith. If you find anything wrong or out of date, please email hello@neurowatch.app and we will correct it.

<!-- Generated from the web app's frontend/src/features/home/comparison-catalog.ts: change that file, then regenerate this one. -->

## Side by side

✅ yes · 🟡 partly · ❌ no

| | NeuroWatch | Frigate | Shinobi | Bluecherry | ZoneMinder | Agent DVR | Viseron | Scrypted | Blue Iris |
|---|---|---|---|---|---|---|---|---|---|
| **Licence** | Apache-2.0, business use included | MIT | GPL Community Edition; paid Pro licence | GPL | GPLv2 | Proprietary, free for private local use | MIT | Its own licence | Proprietary |
| **Cost** | Free for up to 16 cameras; Unlimited USD 0 (normally USD 50) with sign-up; cloud portal optional, with a free plan | Free; optional paid Frigate+ detection models | Community Edition free; Pro licences paid | Free | Free | Free locally; subscription for remote access and business use | Free | Free hub; recorder by subscription | Paid licence |
| **Runs on** | Windows 10 and 11, Linux on 64-bit PCs and ARM, Docker | Linux through Docker; Home Assistant add-on | Linux, or Docker | Linux server; clients for Windows, macOS, Linux and phones | Linux | Windows, macOS, Linux, Docker | Docker | Windows, macOS, Linux, Docker | Windows |
| **Installs like a desktop app** | ✅ Installer, or .deb, .rpm and Arch packages | ❌ Docker, set up with a configuration file | ❌ Install script on a Linux server | 🟡 Server packages plus client apps | ❌ Server packages and a web server | ✅ Installer | ❌ Docker | 🟡 Desktop app for the hub | ✅ Installer |
| **Desktop video wall** | ✅ Multi-monitor walls with drag-and-drop layouts, paging and hardware decoding | ❌ Web interface | ❌ Web interface | ✅ Desktop client | ❌ Web interface | ❌ Web interface | ❌ Web interface | 🟡 Through its apps and smart-home apps | ✅ Windows console |
| **Watch from anywhere without opening ports** | ✅ Optional cloud portal; the recorder connects out | 🟡 Your own VPN or reverse proxy, or Home Assistant's remote access | 🟡 Remote access is yours to set up | 🟡 Clients reach the server through a forwarded port or VPN | 🟡 Remote access is yours to set up | ✅ With a subscription | 🟡 Remote access is yours to set up | ✅ Through its cloud and smart-home apps | 🟡 A forwarded port or VPN |
| **AI object detection** | 🟡 People, cars, dogs and the other everyday objects its model knows (80 kinds), with the free Detect plugin, on the processor, each kind with its own confidence. It runs as its own program, so recording never waits on it; no face recognition | ✅ Its main feature, with many accelerators, plus face recognition | 🟡 Through plugins | ❌ Motion detection | 🟡 Through an add-on | 🟡 Through a separate AI server | ✅ Objects and faces | 🟡 Through plugins | 🟡 Through a separate AI server |
| **Home Assistant or MQTT** | ✅ MQTT with Home Assistant discovery: Motion and Object sensors, camera status and event pictures | ✅ Built for it | 🟡 Through add-ons | ❌ No | 🟡 Through add-ons | ✅ MQTT | ✅ Home Assistant and MQTT | ✅ Home Assistant, Apple Home, Google Home, Alexa | ✅ MQTT |
| **Mobile app** | ❌ A web portal that works on phones; no app store app yet | 🟡 Web interface, or through the Home Assistant app | 🟡 Paid app | ✅ Yes | ✅ zmNinja | ✅ Yes | ❌ Web interface | ✅ Yes | ✅ Yes |
| **Maturity** | New, in beta since 2026 | Since 2019, very active | Since 2016 | Many years; open source since 2019 | Since 2002 | Successor to iSpy | Since 2020 | Several years | Many years |

## Why people choose NeuroWatch

- **It installs like an app.** Download it for Windows or Linux and run the installer. There is no Docker, configuration file or web server to set up, and no account is needed to record.
- **A real video wall.** Watch every camera in a desktop window, across several monitors, with layouts you arrange with the mouse, paging and auto-scroll. Most open-source recorders show cameras only in a browser.
- **It keeps recording on its own.** Recording carries on after you close the window, starts with the computer, reconnects cameras that drop and restarts itself if it ever stops unexpectedly. A tray icon shows when something needs attention.
- **Recording comes first.** Recording is the one job that must never fail, so detection runs beside it, not inside it. Motion and object detection are separate programs: if one crashes, falls behind or is updated, recording carries on, and a detector that falls behind skips pictures instead of holding up the camera. Detection can even run on another computer.
- **Works with Home Assistant.** Point it at your MQTT broker and every camera appears in Home Assistant by itself, with Motion and Object sensors, whether it is online and its latest event picture. Other automation systems can read the same MQTT topics.
- **Light on the computer.** Recording, watching in a browser and exporting clips never decode the video, except to convert H.265 for a browser that can't play it. The recorder decodes only the cameras on screen, on the graphics card where it can, and motion analysis reuses those pictures instead of decoding again.
- **Watch from anywhere, if you want to.** Link the recorder to NeuroWatch Cloud with a one-time code and watch live or recorded video in a browser, H.264 and H.265 alike. The recorder connects out, so no ports are opened, and camera passwords are sealed for the recorder.
- **Your footage stays safe.** It refuses to record into an empty folder when a drive or network share is missing, archives older footage to a NAS and checks every copy, keeps bookmarked footage from cleanup, and can back cameras up off-site with a key only you hold.
- **Open source with a permissive licence.** Apache-2.0, business use included, with no contributor agreement. Free for up to 16 cameras, and the Unlimited edition is currently free with sign-up.

## NeuroWatch beside each one

### NeuroWatch vs Frigate

[Frigate](https://frigate.video): An NVR built around real-time AI object detection, closely tied to Home Assistant.

Choose Frigate if advanced AI detection is the main reason you want a recorder: many accelerators, face recognition, trainable classification and descriptions of what happened go well beyond NeuroWatch's detection, and you are comfortable with Docker and a configuration file.

Choose NeuroWatch if you want to install a recorder on a Windows or Linux PC like any other app, watch a video wall on your own monitors, and reach your cameras from a browser without a VPN or reverse proxy. It works with Home Assistant over MQTT too, and its Detect plugin reports people, cars and the other objects you choose as a separate program, so detection never holds up recording.

### NeuroWatch vs Shinobi

[Shinobi](https://shinobi.video): A web-based NVR with many recording modes, multiple accounts and detector plugins.

Choose Shinobi if you want a browser-based recorder on a Linux server with many recording modes and accounts for several groups of users.

Choose NeuroWatch if you want a desktop app on Windows or Linux, a permissive licence that covers business use without a paid edition, and remote viewing that works without opening ports.

### NeuroWatch vs Bluecherry

[Bluecherry](https://www.bluecherrydvr.com): A client and server recorder, open source since 2019, with apps for desktop and phone.

Choose Bluecherry if you want a classic client and server setup with a Linux server, desktop clients including macOS, and mobile apps.

Choose NeuroWatch if you want the recorder and the video wall on the same Windows or Linux computer, a browser portal that needs no open ports, plugin-based analysis and off-site backup.

### NeuroWatch vs ZoneMinder

[ZoneMinder](https://zoneminder.com): The long-standing open-source recorder for Linux servers, developed since 2002.

Choose ZoneMinder if you want the most established open-source recorder for Linux servers, with decades of community knowledge, and you are happy to administer a web server.

Choose NeuroWatch if you want something you install and use like an app, on Windows or Linux, with a desktop video wall, alerts and browser access ready to go.

### NeuroWatch vs Agent DVR

[Agent DVR](https://www.ispyconnect.com): The successor to iSpy, free locally, with a subscription for remote access and business use.

Choose Agent DVR if you want a polished recorder on Windows, macOS or Linux, its integrations with AI servers, and you are happy to pay for remote access or business use.

Choose NeuroWatch if you want an open-source recorder that businesses can use for free, with a desktop video wall and a browser portal whose free plan includes remote viewing.

### NeuroWatch vs Viseron

[Viseron](https://viseron.netlify.app): A self-hosted recorder focused on object and face detection, including on a CPU alone.

Choose Viseron if you want a small, MIT-licensed recorder in Docker focused on object and face detection.

Choose NeuroWatch if you want a desktop app with a video wall, recording schedules, playback with a timeline and browser access without your own server setup, with people, cars and other objects reported by its Detect plugin.

### NeuroWatch vs Scrypted

[Scrypted](https://www.scrypted.app): A camera hub for Apple Home, Google Home and Alexa; its recorder is a paid add-on.

Choose Scrypted if your cameras need to appear in Apple Home, Google Home or Alexa, including HomeKit Secure Video.

Choose NeuroWatch if you want an open-source recorder in its own right, recording to your own drives with a desktop wall, rather than a bridge to a smart-home ecosystem.

### NeuroWatch vs Blue Iris

[Blue Iris](https://blueirissoftware.com): A long-established paid recorder for Windows, included here as the usual commercial reference.

Choose Blue Iris if you want a long-established Windows recorder with a large user community and you are happy with a paid, closed-source licence.

Choose NeuroWatch if you want an open-source alternative that runs on Windows and Linux, is free for up to 16 cameras, and adds a browser portal without port forwarding.

## Questions

### What is the best open-source NVR for Windows?

Most open-source recorders, such as Frigate, Shinobi, ZoneMinder and Viseron, are built for Linux servers or Docker. NeuroWatch installs on Windows 10 and 11 and on Linux like any other app, with a desktop video wall, and is open source under Apache-2.0. Agent DVR and Blue Iris also run on Windows but are not open source.

### Is NeuroWatch an alternative to Frigate?

For many people, yes, but they suit different needs. Frigate is the stronger choice for advanced AI object detection, with face recognition, accelerators and trainable models. NeuroWatch is the easier choice if you want an installed app on Windows or Linux, a desktop video wall and browser access without setting up a VPN. Both work with Home Assistant over MQTT, and NeuroWatch's free Detect plugin reports people, cars and the other objects you choose, running beside the recorder so recording never depends on it.

### Does NeuroWatch work with Home Assistant?

Yes. In Settings → Home Assistant, enter your MQTT broker (for example the Mosquitto add-on) and each camera appears in Home Assistant by itself: Motion and Object sensors, whether the camera is online, and its latest event picture, plus a problem sensor for the recorder. Any other MQTT system can read the same topics, including every event as JSON.

### Why is object detection a separate plugin in NeuroWatch?

Because recording comes first. Writing the footage is the one job a recorder must not fail at, and AI detection is its heaviest and most often changed part, with large models, graphics drivers and frequent updates. NeuroWatch keeps detection out of the recording process: it runs as its own program and reads the compressed video through its own bounded queue. If it crashes, the recorder reports the plugin offline and keeps recording. If it falls behind, it skips pictures rather than holding up the camera or recording. It can be updated, restarted or switched off without touching recording, and it can run on another computer, connected over encrypted QUIC or TLS, so a busy recorder does not have to do the detection. The cost is that Detect is a separate download today, and its detection is simpler than Frigate's.

### Is NeuroWatch an open-source alternative to Blue Iris?

Yes. Like Blue Iris it runs on Windows with a desktop view of your cameras, recording in the background and playback on a timeline. Unlike Blue Iris it is open source, also runs on Linux, is free for up to 16 cameras and offers browser access without port forwarding. Blue Iris has a much longer track record; NeuroWatch is in beta.

### How is NeuroWatch different from Shinobi and ZoneMinder?

Shinobi and ZoneMinder are server programs you use through a web browser, mainly on Linux. NeuroWatch is a desktop app for Windows and Linux with its own video wall that keeps recording as a background service, plus an optional browser portal. Its Apache-2.0 licence covers business use without a paid edition.

### How is NeuroWatch different from Bluecherry?

Bluecherry runs a recorder on a Linux server that desktop and mobile clients connect to. NeuroWatch records and shows its video wall on the same Windows or Linux computer, and reaches browsers through NeuroWatch Cloud without opening ports. Bluecherry has mobile apps and a macOS client; NeuroWatch has neither yet.

### Can I watch my cameras remotely without port forwarding?

With NeuroWatch, yes: link the recorder to NeuroWatch Cloud with a one-time code and it connects out, so nothing on the internet connects into your network. Linking is optional and recording never depends on it.

### What can't NeuroWatch do that others can?

Its object detection is basic: the Detect plugin reports people, cars and the other everyday objects its model knows, on the processor, with no face recognition, trainable models or AI accelerators (Frigate and Viseron lead there). It has no app store mobile app, no macOS version and no email alerts, and Home Assistant cannot yet control it, only read its sensors and events. It is also new and in beta, while ZoneMinder, Bluecherry and Blue Iris have been used for many years.

### Who wrote this comparison?

The NeuroWatch team, from each project's own public documentation and release notes, checked on 14 November 2026. Projects change quickly; if something here is wrong or out of date, please email hello@neurowatch.app and it will be corrected.

---

Product names belong to their owners and are used here only to identify those products.
