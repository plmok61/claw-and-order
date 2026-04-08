import {
  Component,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

type SessionStatus = "busy" | "idle" | "unknown";

export interface SessionSummary {
  path: string;
  session_id: string;
  project_label: string;
  cwd: string | null;
  title: string | null;
  status: SessionStatus;
  busy_since: string | null;
  completed: boolean;
  mtime_ms: number;
  needs_input_hook: boolean;
  needs_your_attention: boolean;
}

const RECENT_MS = 30 * 60 * 1000;
const TOOL_PREVIEW_CHARS = 6000;
const SIDEBAR_NOTICE_MAX = 360;

function truncateNoticeText(text: string, max = SIDEBAR_NOTICE_MAX): string {
  const t = text.trim();
  if (t.length <= max) return t;
  return `${t.slice(0, max)}…`;
}

/** Tool/user payloads are sometimes strings, sometimes structured blocks; React cannot render objects as children. */
function contentToDisplayString(value: unknown, maxChars: number): string {
  if (value === null || value === undefined) return "";
  if (typeof value === "string") {
    return value.length > maxChars
      ? `${value.slice(0, maxChars)}\n… (${value.length} chars)`
      : value;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    const parts = value.map((item) => {
      if (item !== null && typeof item === "object" && "text" in item) {
        return String((item as { text?: unknown }).text ?? "");
      }
      try {
        return JSON.stringify(item);
      } catch {
        return String(item);
      }
    });
    return contentToDisplayString(parts.join("\n"), maxChars);
  }
  try {
    const s = JSON.stringify(value, null, 2);
    return contentToDisplayString(s, maxChars);
  } catch {
    return String(value);
  }
}

function safeJsonStringify(value: unknown, maxLen: number): string {
  try {
    const s = JSON.stringify(value, null, 2);
    return s.length > maxLen ? `${s.slice(0, maxLen)}\n… (truncated)` : s;
  } catch {
    return "[unserializable]";
  }
}

class TranscriptRowErrorBoundary extends Component<
  { children: ReactNode; lineIndex: number },
  { error: Error | null }
> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (this.state.error) {
      return (
        <div className="transcript-block transcript-error">
          <span className="transcript-label">Render error (line {this.props.lineIndex})</span>
          <pre className="transcript-body">{this.state.error.message}</pre>
        </div>
      );
    }
    return this.props.children;
  }
}

function formatElapsed(iso: string | null, now: number): string {
  if (!iso) return "—";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "—";
  let sec = Math.max(0, Math.floor((now - t) / 1000));
  const h = Math.floor(sec / 3600);
  sec %= 3600;
  const m = Math.floor(sec / 60);
  const s = sec % 60;
  if (h > 0) return `${h}:${String(m).padStart(2, "0")}:${String(s).padStart(2, "0")}`;
  return `${m}:${String(s).padStart(2, "0")}`;
}

function isRecent(mtimeMs: number, now: number): boolean {
  return now - mtimeMs < RECENT_MS;
}

function displayDirName(s: SessionSummary): string {
  const cwd = s.cwd ?? "";
  const trimmed = cwd.replace(/\/+$/, "");
  const base = trimmed.split("/").pop() ?? "";
  const name = base.trim();
  if (name) return name;
  return s.project_label || s.session_id.slice(0, 8);
}

function TranscriptEntry({ row }: { row: Record<string, unknown> }) {
  const t = row.type as string | undefined;
  if (t === "file-history-snapshot") return null;

  if (t === "user" && row.toolUseResult) {
    const tr = row.toolUseResult as {
      content?: unknown;
      is_error?: boolean;
      tool_use_id?: string;
    };
    const clipped = contentToDisplayString(tr.content, TOOL_PREVIEW_CHARS);
    return (
      <div
        className={`transcript-block tool-result ${tr.is_error ? "error" : ""}`}
      >
        <span className="transcript-label">
          Tool result (user){tr.tool_use_id ? ` · ${tr.tool_use_id}` : ""}
        </span>
        <pre className="transcript-body">{clipped}</pre>
      </div>
    );
  }

  if (t === "user") {
    const meta = row.isMeta === true;
    const content = (row.message as { content?: unknown } | undefined)?.content;
    const text =
      typeof content === "string"
        ? content
        : Array.isArray(content)
          ? content
              .map((b) =>
                typeof b === "object" && b && "text" in b
                  ? String((b as { text?: string }).text ?? "")
                  : "",
              )
              .join("")
          : "";
    if (!text.trim()) return null;
    return (
      <div className={`transcript-block user ${meta ? "meta" : ""}`}>
        <span className="transcript-label">{meta ? "User (meta)" : "You"}</span>
        <pre className="transcript-body">{text}</pre>
      </div>
    );
  }

  if (t === "assistant") {
    const msg = row.message as {
      content?: unknown;
      model?: string;
      error?: string;
    };
    const blocks = msg?.content;
    const parts: ReactNode[] = [];
    if (typeof blocks === "string") {
      parts.push(
        <pre key={0} className="transcript-body assistant-text">
          {blocks}
        </pre>,
      );
    } else if (Array.isArray(blocks)) {
      for (let i = 0; i < blocks.length; i++) {
        const raw = blocks[i];
        if (raw === null || typeof raw !== "object") continue;
        const b = raw as Record<string, unknown>;
        const bt = b.type as string | undefined;
        if (bt === "text") {
          parts.push(
            <pre key={i} className="transcript-body assistant-text">
              {String(b.text ?? "")}
            </pre>,
          );
        } else if (bt === "thinking") {
          parts.push(
            <details key={i} className="thinking">
              <summary>Thinking</summary>
              <pre>{String(b.thinking ?? "")}</pre>
            </details>,
          );
        } else if (bt === "tool_use") {
          const input = b.input;
          let inputStr: string;
          if (typeof input === "string") {
            inputStr = input;
          } else {
            try {
              inputStr = JSON.stringify(input, null, 2);
            } catch {
              inputStr = String(input);
            }
          }
          const clipped = contentToDisplayString(inputStr, TOOL_PREVIEW_CHARS);
          parts.push(
            <div key={i} className="tool-use">
              <span className="tool-name">{String(b.name ?? "tool")}</span>
              <pre className="transcript-body">{clipped}</pre>
            </div>,
          );
        }
      }
    }
    return (
      <div className="transcript-block assistant">
        <span className="transcript-label">
          Assistant{msg?.model ? ` · ${msg.model}` : ""}
          {msg?.error ? ` · ${msg.error}` : ""}
        </span>
        {parts}
      </div>
    );
  }

  if (t === "tool_result") {
    const tr = row.toolUseResult as
      | { content?: unknown; is_error?: boolean; tool_use_id?: string }
      | undefined;
    const clipped = contentToDisplayString(tr?.content, TOOL_PREVIEW_CHARS);
    return (
      <div
        className={`transcript-block tool-result ${tr?.is_error ? "error" : ""}`}
      >
        <span className="transcript-label">
          Tool result{tr?.tool_use_id ? ` · ${tr.tool_use_id}` : ""}
        </span>
        <pre className="transcript-body">{clipped}</pre>
      </div>
    );
  }

  if (t === "system") {
    return (
      <details className="transcript-block system">
        <summary>System</summary>
        <pre className="transcript-body small">{safeJsonStringify(row, 8000)}</pre>
      </details>
    );
  }

  if (t === "summary") {
    return (
      <div className="transcript-block summary">
        <span className="transcript-label">Compaction summary</span>
        <pre className="transcript-body small">{safeJsonStringify(row, 120_000)}</pre>
      </div>
    );
  }

  if (t === "result") {
    return (
      <div className="transcript-block result">
        <span className="transcript-label">Session result</span>
        <pre className="transcript-body small">{safeJsonStringify(row, 120_000)}</pre>
      </div>
    );
  }

  return (
    <details className="transcript-block raw">
      <summary>Raw ({t ?? "?"})</summary>
      <pre className="transcript-body small">{safeJsonStringify(row, 120_000)}</pre>
    </details>
  );
}

function App() {
  const [root, setRoot] = useState<string | null | undefined>(undefined);
  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  const [transcript, setTranscript] = useState<Record<string, unknown>[] | null>(
    null,
  );
  const [transcriptErr, setTranscriptErr] = useState<string | null>(null);
  const [recentOnly, setRecentOnly] = useState(false);
  const [tick, setTick] = useState(() => Date.now());
  const [transcriptBump, setTranscriptBump] = useState(0);
  const [sidebarNotice, setSidebarNotice] = useState<{
    kind: "ok" | "err";
    text: string;
  } | null>(null);
  const [terminalOpeningPath, setTerminalOpeningPath] = useState<string | null>(
    null,
  );
  const transcriptScrollRef = useRef<HTMLDivElement | null>(null);

  const refresh = useCallback(async () => {
    try {
      const r = await invoke<string | null>("get_projects_root");
      setRoot(r);
      const list = await invoke<SessionSummary[]>("list_sessions");
      setSessions(list);
    } catch {
      setRoot(null);
      setSessions([]);
    }
  }, []);

  useEffect(() => {
    void refresh();
    const id = window.setInterval(() => setTick(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [refresh]);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    void listen("sessions-changed", () => {
      void refresh();
      setTranscriptBump((n) => n + 1);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => {
      unlisten?.();
    };
  }, [refresh]);

  useEffect(() => {
    if (!sidebarNotice) return;
    const t = window.setTimeout(() => setSidebarNotice(null), 6000);
    return () => window.clearTimeout(t);
  }, [sidebarNotice]);

  const openSessionTerminal = useCallback(async (s: SessionSummary) => {
    setTerminalOpeningPath(s.path);
    try {
      const msg = await invoke<string>("focus_session_terminal", {
        sessionJsonlPath: s.path,
        cwd: s.cwd,
      });
      setSidebarNotice({ kind: "ok", text: msg });
    } catch (err) {
      setSidebarNotice({ kind: "err", text: String(err) });
    } finally {
      setTerminalOpeningPath(null);
    }
  }, []);

  useEffect(() => {
    if (!selectedPath) {
      setTranscript(null);
      setTranscriptErr(null);
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const rows = await invoke<Record<string, unknown>[]>(
          "read_session_transcript",
          { path: selectedPath },
        );
        if (!cancelled) {
          setTranscript(rows);
          setTranscriptErr(null);
        }
      } catch (e) {
        if (!cancelled) {
          setTranscript(null);
          setTranscriptErr(String(e));
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selectedPath, transcriptBump]);

  useEffect(() => {
    const el = transcriptScrollRef.current;
    if (!el) return;
    if (!transcript || transcriptErr) return;
    // Scroll to bottom after transcript renders/updates.
    requestAnimationFrame(() => {
      el.scrollTop = el.scrollHeight;
    });
  }, [selectedPath, transcriptBump, transcript, transcriptErr]);

  const filtered = useMemo(() => {
    if (!recentOnly) return sessions;
    return sessions.filter((s) => isRecent(s.mtime_ms, tick));
  }, [sessions, recentOnly, tick]);

  useEffect(() => {
    if (!sessions.length) {
      setSelectedPath(null);
      return;
    }
    if (!filtered.length) {
      if (recentOnly) setSelectedPath(null);
      return;
    }
    if (!selectedPath || !filtered.some((s) => s.path === selectedPath)) {
      setSelectedPath(filtered[0].path);
    }
  }, [sessions, filtered, selectedPath, recentOnly]);

  const selected =
    sessions.find((s) => s.path === selectedPath) ??
    filtered.find((s) => s.path === selectedPath) ??
    null;

  return (
    <div className="shell">
      <aside className="sidebar">
        <header className="sidebar-header">
          <h1>Sessions</h1>
          <label className="filter-toggle">
            <input
              type="checkbox"
              checked={recentOnly}
              onChange={(e) => setRecentOnly(e.target.checked)}
            />
            Last 30 min
          </label>
        </header>
        {root === undefined && <p className="muted">Loading…</p>}
        {root === null && (
          <p className="warn">
            Could not resolve home directory or Claude data path.
          </p>
        )}
        {root !== undefined && root !== null && !sessions.length && (
          <p className="muted">
            No session files under <code>{root}</code>. Run Claude Code in a
            project first.
          </p>
        )}
        {recentOnly && filtered.length === 0 && sessions.length > 0 && (
          <p className="muted">No sessions in the last 30 minutes.</p>
        )}
        {sidebarNotice && (
          <p
            className={
              sidebarNotice.kind === "ok" ? "sidebar-notice ok" : "sidebar-notice err"
            }
            title={
              sidebarNotice.text.length > SIDEBAR_NOTICE_MAX
                ? sidebarNotice.text
                : undefined
            }
          >
            {truncateNoticeText(sidebarNotice.text)}
          </p>
        )}
        <ul className="session-list">
          {filtered.map((s) => {
            const sel = s.path === selectedPath;
            const recent = isRecent(s.mtime_ms, tick);
            const opening = terminalOpeningPath === s.path;
            const needsYou = s.needs_your_attention && !s.completed;
            const working = s.status === "busy";
            return (
              <li key={s.path} className="session-list-item">
                <button
                  type="button"
                  className={`session-item ${sel ? "selected" : ""} ${!recent ? "stale" : ""}${
                    needsYou ? " session-item--needs-you" : ""
                  }${working ? " session-item--working" : ""}`}
                  onClick={() => setSelectedPath(s.path)}
                >
                  <div className="session-row-top">
                    <span
                      className={
                        needsYou
                          ? "status-dot status-needs-you"
                          : `status-dot status-${s.status}`
                      }
                      title={
                        s.needs_input_hook
                          ? "Hook: needs input"
                          : s.needs_your_attention
                            ? "Likely waiting for you"
                            : s.status
                      }
                    />
                    <span className="session-title">
                      {displayDirName(s)}
                    </span>
                  </div>
                  <div className="session-meta">
                    <span className="project-tag">{s.project_label}</span>
                    {working && (
                      <>
                        <span className="badge working">working</span>
                        <span className="timer" title="Approx. time in current busy state">
                          {formatElapsed(s.busy_since, tick)}
                        </span>
                      </>
                    )}
                    {s.completed && <span className="badge done">done</span>}
                    {needsYou && (
                      <span className="badge need">needs you</span>
                    )}
                  </div>
                </button>
                <button
                  type="button"
                  className="session-terminal-btn"
                  disabled={opening}
                  title="Focus Terminal, iTerm, Cursor, or Warp if this session file is open; otherwise open Terminal at the project folder."
                  aria-label="Go to terminal for this session"
                  onClick={(e) => {
                    e.stopPropagation();
                    void openSessionTerminal(s);
                  }}
                >
                  {opening ? (
                    <span className="terminal-btn-spinner" aria-hidden />
                  ) : (
                    <svg
                      className="terminal-btn-icon"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      aria-hidden
                    >
                      <rect x="2" y="4" width="20" height="16" rx="2" />
                      <path d="M6 8l4 4-4 4M11 16h6" />
                    </svg>
                  )}
                </button>
              </li>
            );
          })}
        </ul>
      </aside>
      <main className="main">
        {!selected && (
          <div className="empty-main">
            <p>Select a session to view its transcript.</p>
            <p className="muted small">
              Status and timers are inferred from JSONL (best effort). They may
              differ from the Claude TUI.
            </p>
          </div>
        )}
        {selected && (
          <>
            <header className="main-header">
              <h2>{selected.title ?? selected.session_id}</h2>
              {selected.cwd && (
                <p className="cwd">
                  <code>{selected.cwd}</code>
                </p>
              )}
            </header>
            <div className="transcript-scroll" ref={transcriptScrollRef}>
              {transcriptErr && (
                <p className="warn">Could not load transcript: {transcriptErr}</p>
              )}
              {transcript &&
                transcript.map((row, i) => (
                  <TranscriptRowErrorBoundary key={i} lineIndex={i}>
                    <TranscriptEntry row={row} />
                  </TranscriptRowErrorBoundary>
                ))}
            </div>
          </>
        )}
      </main>
    </div>
  );
}

export default App;
