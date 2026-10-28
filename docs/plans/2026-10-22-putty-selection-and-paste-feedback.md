# PuTTY selection and paste feedback

Baseline during investigation: 5f9d637700478c68fdd9df7e810d2f7abbfa3f45.
The user's executable reports 04680fedd+dirty, so the v5.2.4 banner alone does
not establish that this is an old build.

PuTTY commonly reports TERM=xterm, selecting 16-colour rendering. The selected
text band uses a grey PanelSelBg; nearest-colour quantization can choose the
same black as the screen. This makes an application selection look absent even
though release emits an SSH copy request. Use the existing reverse-video
selection for 16-colour/monochrome or background-less terminals. Preserve the
uniform band for 256-colour and truecolour terminals and keep selected text
extraction unchanged. This does not change mouse capture defaults.

Ctrl+V delivered as a key is bound to AttachImage. Over SSH the remote process
cannot read the client's system clipboard, so a missing image error is the
wrong response to an ordinary paste attempt. Show a replaceable, non-error tip
before reading any clipboard: PuTTY Shift+Insert, Mac Cmd+V. Explicit /paste
image requests retain their image-specific failure. Never read the server's
clipboard in place of the client's or claim a successful paste without text.

Reference: https://www.puttyssh.org/0.78/htmldoc/Chapter3.html#using-copypaste
PuTTY native selection/paste can use Shift while application mouse reporting
is active. OSC 52 copy support/acceptance still requires client verification.

Tests check selection style and extracted text by terminal capability, and
repeated remote Ctrl+V preserving the draft, producing no transcript entries
and making no clipboard reads. Actual PuTTY testing remains necessary.

Validation: 1,827 TUI tests passed, including the two new regressions. The full
compile gate, formatting and diff checks passed.
