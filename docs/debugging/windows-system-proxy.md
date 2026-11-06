# Windows system proxy refresh failures

When `/proxy` uses `follow_system`, model requests read the current user's
`Software\Microsoft\Windows\CurrentVersion\Internet Settings` before opening HTTP.
Switching a model or session can expose the error but is not required to trigger it.

Errors identify the field and OS code, for example:

```text
could not refresh system proxy settings: ProxyEnable: invalid value or registry type (OS error 222)
```

- `Windows Internet Settings`: opening the registry key failed.
- `ProxyEnable`: reading the enable flag failed or it changed during the read.
- `ProxyServer`: an enabled proxy has a missing/empty/unreadable endpoint, or no
  supported HTTP/HTTPS route.
- `ProxyOverride`: reading the bypass list failed. A missing bypass value is valid.
- OS error 5 means access denied; 222 is the type conversion failure returned by
  winreg. No raw registry contents or credential-bearing proxy URL is included.

Missing/empty enabled endpoints, changed flags and selected transient OS failures trigger
at most three reads with 25ms and 75ms waits. Permanent failures stop immediately;
exhausted Windows reads do not consume the model's five outer retries. No failed
read authorizes an automatic direct route or overwrites saved proxy settings.

If the error persists, check the system proxy application's settings. A fixed
proxy can be selected through `/proxy` when that route is known. Select `no_proxy`
only when direct access is intended. Report the AtomCode version, affected field,
OS code, and whether restarting the proxy application changes the outcome.
Do not include proxy credentials in the report.

These changes recover short configuration transitions and make persistent
failures diagnosable. They do not establish the root cause of every report that
previously displayed only `could not refresh system proxy settings`.

## Loopback connection probes

Reading system settings and probing a configured loopback proxy are separate
stages. A definite connection refusal may temporarily select direct access in
follow-system mode. A timeout, permission failure or unknown socket error keeps
the proxy route: a listening proxy can time out when its accept queue is full.

The Windows probe waits on both writable and exception Winsock sets and reads
SO_ERROR from the same socket. A polling timeout with a pending refusal can then
be distinguished from a genuine timeout. Fixed proxy mode is never bypassed.
