# Privacy

Downpour runs locally. The desktop app stores its queue and settings in its
operating-system app-data directory so downloads can be resumed after restart.

The optional browser extension is disabled by default. When you enable capture,
only downloads started after that point are sent to the locally running app at
`http://127.0.0.1:53472`. The extension does not request browsing-history,
cookie, or all-sites permissions, and it does not transmit data to a remote
service.

The app removes directory components from browser-supplied filenames before
storing them, so local paths are not retained in the queue. Download URLs and
file names may still be personal data; do not include the app-data directory or
screenshots containing them in bug reports unless you have reviewed them.

For a source release, `.env` files, certificates, keys, local workspace notes,
build outputs, and temporary download files are ignored by Git.
