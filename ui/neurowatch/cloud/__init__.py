"""NeuroWatch Cloud in the desktop app, the tray and the headless commands.

``desktop`` (the status bar light; Qt), ``link_dialog`` and ``cloud_link``
(linking, also used by Settings → Cloud), ``tray`` and ``headless`` (no Qt:
they run on the tray's worker thread and on servers without a display).
"""
