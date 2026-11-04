# Desktop update notices

The desktop checks the public release catalog at
`https://neurowatch.app/v1/downloads?version=latest` when it starts and every
hour. It offers a newer stable semantic version; development commit labels
are not comparable and prereleases are not advertised as stable upgrades.

`models/updates.py` bounds the HTTPS request to 8 seconds and 256 KiB.
`widgets/update_notice.py` runs one check at a time on a worker and installs a
keyboard-accessible status-bar button next to the current version. Clicking
opens the downloads page with the new version and recorder edition selected.
No account, cloud link, camera data or cookies are required for the check.
There is no automatic installation or restart. A failed check is quiet and
retains a previously found update. Window shutdown stops the timer and retires
an outstanding bounded worker without blocking recording.

Tests in `tests/ui/test_update_notice.py` cover version comparison, malformed
catalogs, bounded requests, the hourly timer, offline behaviour, window
integration and keyboard navigation. UI test fixtures block real update
requests. The web app shares the catalog with its normal downloads page;
Unlimited is currently USD 0 (normally USD 50) after account sign-up.
