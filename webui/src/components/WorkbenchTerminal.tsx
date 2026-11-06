import { useEffect, useRef, useState } from "preact/hooks";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import "@xterm/xterm/css/xterm.css";
import { ApiCallError, workbenchRequest } from "../api";
import { useSettings } from "../settings";

export function WorkbenchTerminal({
  session,
  id,
  onRestart,
}: {
  session: string;
  id: string;
  onRestart: () => Promise<void>;
}) {
  const { lang } = useSettings();
  const zh = lang === "zh";
  const host = useRef<HTMLDivElement>(null);
  const [error, setError] = useState("");
  const [exited, setExited] = useState(false);
  const [restarting, setRestarting] = useState(false);
  useEffect(() => {
    let finished = false;
    const term = new Terminal({
      cursorBlink: true,
      fontSize: 13,
      scrollback: 3000,
      theme: { background: "#16181d", foreground: "#e6e6eb" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.open(host.current!);
    let stopped = false;
    let timer: ReturnType<typeof setTimeout>;
    let offset = 0;
    let writes = Promise.resolve();
    let pendingWrites = 0;
    const send = (body: object) => {
      // Keep input bounded even when a proxy stalls. Never retry uncertain input.
      if (pendingWrites >= 128) {
        setError(
          zh
            ? "终端输入队列已满，请等待连接恢复。"
            : "Terminal input queue is full; wait for the connection.",
        );
        return;
      }
      pendingWrites++;
      writes = writes
        .then(() => {
          if (!stopped)
            return workbenchRequest<void>("terminals/" + id, {
              session_id: session,
              ...body,
            });
        })
        .catch((e) => {
          if (!stopped) setError(String(e));
        })
        .finally(() => {
          pendingWrites--;
        });
    };
    let buffered = "";
    let inputTimer: ReturnType<typeof setTimeout> | undefined;
    const input = term.onData((data) => {
      if (new TextEncoder().encode(buffered + data).length > 16384) {
        setError(
          zh
            ? "粘贴内容超过终端输入上限（16 KiB）。"
            : "Paste exceeds the 16 KiB terminal input limit.",
        );
        return;
      }
      if (finished) return;
      buffered += data;
      if (!inputTimer)
        inputTimer = setTimeout(() => {
          send({ input: buffered });
          buffered = "";
          inputTimer = undefined;
        }, 20);
    });
    const observer = new ResizeObserver(() => {
      if (!host.current?.clientWidth || !host.current?.clientHeight) return;
      fit.fit();
      send({ cols: term.cols, rows: term.rows });
    });
    observer.observe(host.current!);
    async function poll() {
      try {
        const result = await workbenchRequest<{
          bytes: number[];
          offset: number;
          truncated: boolean;
          finished: boolean;
          exit_code: number | null;
        }>(
          "terminals/" +
            id +
            "?" +
            new URLSearchParams({
              session_id: session,
              offset: String(offset),
            }),
        );
        if (stopped) return;
        if (result.truncated)
          term.write(
            zh
              ? "\r\n[较早的终端输出已过期]\r\n"
              : "\r\n[Earlier terminal output expired]\r\n",
          );
        term.write(new Uint8Array(result.bytes));
        offset = result.offset;
        if (result.finished && result.bytes.length === 0 && result.exit_code !== null) {
          finished = true;
          setExited(true);
          term.write(`\r\n[${zh ? "进程已退出，退出码" : "Process exited, code"}: ${result.exit_code}]\r\n`);
          return;
        }
        timer = setTimeout(poll, result.bytes.length === 65536 ? 0 : 250);
      } catch (e) {
        if (!stopped) {
          setError(String(e));
          if (!(e instanceof ApiCallError && e.status === 404))
            timer = setTimeout(poll, 2000);
        }
      }
    }
    void poll();
    return () => {
      stopped = true;
      clearTimeout(timer);
      clearTimeout(inputTimer);
      input.dispose();
      observer.disconnect();
      term.dispose();
    };
  }, [session, id]);
  return (
    <div class="workbench-terminal">
      {exited && <button disabled={restarting} onClick={async () => {
        setRestarting(true);
        try { await onRestart(); } catch (e) { setError(String(e)); }
        finally { setRestarting(false); }
      }}>{zh ? "重新启动终端" : "Restart terminal"}</button>}
      <div ref={host} class="workbench-terminal-host" />
      {error && (
        <div role="alert" class="workbench-error">
          {error}
        </div>
      )}
    </div>
  );
}
