from neurowatch.service.paths import restore_host_environment

# Programs this packaged app starts (systemctl, loginctl, the URL opener) get
# the system's own libraries, not the bundle's (see host_environment).
restore_host_environment()

from neurowatch.main import main  # noqa: E402

if __name__ == "__main__":
    main()
