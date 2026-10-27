# WebUI relay and mobile recovery implementation plan

Goal: recover from transient relay authorization failures without losing page
state, preserve scroll intent on mobile resume, and compact the unfocused
mobile composer while keeping Stop reachable.

Baseline: release/v5.2.5, 60173252c3a2d6de53c61258017da1bddb8eb280.

Architecture: retain the existing daemon token authority and browser-only relay
credential transport. Relay credentials stay in sessionStorage/in-memory; never
move them to localStorage, cookies or URL queries, and never replay mutations.
A protected project probe distinguishes permission rejection from transport
failure. Authentication retry must not unmount an already-open App. Chat owns
scroll intent; composer compaction is responsive presentation only, with the
existing Stop action and expanded controls retained.

Tech stack: Preact, TypeScript, CSS; existing Node test runner and Rust nextest.

## Findings and boundaries

- Daemon WebuiTokenStore has no timed expiration. Relay disconnection normally
  returns 503, whereas missing/invalid access tokens return 401. Production
  response/log capture is still needed to establish the initiating cause.
- Pairing fragments were removed even after sessionStorage writes failed. A
  later reload/OS page eviction could lose a previously working link. Only
  scrub after reading back the exact pairing; otherwise retain the original
  fragment (never a query). Storage clearing after a successful handoff and
  actual server token invalidation still require a fresh pairing link.
- Root currently latches every unauthorized event permanently and unmounts App.
  There is no recovery probe and the message conflates missing pairing with a
  rejected existing pairing. Login status polling can overlap and can announce
  a late result after unmount.
- Chat follows only on content updates. Background viewport/scroll changes can
  release follow with no user scroll; foreground return does not restore intent.
- The mobile composer always renders a multi-row textarea, toolbar and metadata.

## Work

1. Add explicit allowed/denied/unavailable access probing and a bounded,
   single-flight recovery controller. Revalidate unauthorized events against a
   protected endpoint, retry denial on timer/foreground/online and from a button,
   keep an authenticated App mounted while access is denied, and retain the
   missing-link instruction for real unpaired pages. Never treat a failed probe
   as renewed authorization. Avoid overlapping login status polls and ignore
   stale asynchronous responses.
2. Store scroll intent before backgrounding. On visible/pageshow/viewport changes
   restore bottom only if following before suspension, or restore the saved
   history position. Use scroll-container coordinates rather than scrolling the
   whole document. Observe layout growth while following. User scrolls must win
   over later restoration work.
3. At mobile widths, collapse the unfocused composer to one row. Keep Stop on
   the right for active tasks, reveal existing controls on focus, preserve draft,
   attachments, errors and accessible keyboard/touch actions. Desktop unchanged.
4. Test access failure/recovery and missing credentials, no mutation retries,
   credential confidentiality, scroll resume vs history reading, and compact
   composer idle/running/focus states. Run frontend tests, typecheck and build;
   affected Rust tests/full compile gate if backend auth responses change.

Unrelated dirty provider/team/docs work is preserved. No commit/push is implied.

## Verification and remaining scope

Local isolated Chrome testing uses fake relay/API responses, not production
credentials. Confirmed: 390px composer is 50px tall with a 44px Stop button;
focus expands controls; Stop reaches the existing endpoint without losing the
draft. Desktop toolbar remains expanded. Simulated tab hide/show restores both
bottom-follow and history reading. An account-endpoint 401 with a valid project
probe keeps the page; actual rejection covers the mounted page and revalidation
restores the draft. Storage-write failure retains the link across reload; an
unpaired page makes no protected requests. No browser page errors.

The built-in interactive browser connection was unavailable, so these checks
used a separate local test browser with mocked responses. Actual iOS/Safari
page eviction and the production relay initiating 401 still need device/service
verification. No claim is made that revoked tokens can be restored without a
new terminal link. The controller never replays submit/approval/stop operations.

Final checks: all 304 frontend tests passed; TypeScript typecheck, production
build, cargo fmt/check and git diff whitespace check passed. Local Chrome touch
Stop and idle collapsed-composer checks also passed with no page errors. No
backend files or dependencies changed by this task.

Pre-commit audit: authentication/credential handling, retry disposal, session
switch scroll isolation and mobile Stop behavior reviewed without blocking
findings. `bash gates/compile.sh` passed, including both feature and canary
checks. Cargo.lock is unchanged and contains none of the three prohibited
private-overlay package names.
