# WebUI LAN access QR

The daemon launcher owns listener reuse, the actual port, token minting and URL selection. Return the existing report and a separate optional QR URL from that owner; retain the string launcher wrapper for existing callers. The TUI command uses the same UserInterface::show_qr seam as /login, retaining the text URL. No token is logged or added to configuration.

Only a non-loopback usable access address gets a QR. Reusing a loopback listener, failing to bind, or failing LAN address discovery does not produce a mobile QR. Existing browser-opening, sync parameters and authentication remain unchanged. A missing UI seam falls back to the existing text report.

Verify URL eligibility and preservation of token/sync/port, and command presentation with a recording UI. Existing QR rendering tests cover dimensions, contrast and narrow-terminal fallback.
