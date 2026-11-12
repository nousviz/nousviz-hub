import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./App.css";

interface InstanceView {
  id: string;
  name: string;
  url: string;
  email: string | null;
  has_token: boolean;
  token_in_fallback: boolean;
}

interface Health {
  state: "checking" | "healthy" | "degraded" | "unreachable";
  version?: string;
  detail?: string;
}

interface LocalStatus {
  supported: boolean;
  installed: boolean;
  running: boolean;
  setup_running: boolean;
  version: string | null;
  url: string | null;
}

type StepStatus = "pending" | "running" | "ok" | "error";

const SETUP_STEPS: { id: string; label: string }[] = [
  { id: "uv", label: "Download uv (Python manager)" },
  { id: "python", label: "Install Python 3.12" },
  { id: "node", label: "Install Node.js" },
  { id: "postgres", label: "Set up PostgreSQL database" },
  { id: "release", label: "Download NousViz" },
  { id: "configure", label: "Install dependencies & migrate" },
  { id: "build", label: "Build the web interface" },
  { id: "start", label: "Start services" },
];

function LocalPanel({ onInstancesChanged }: { onInstancesChanged: () => void }) {
  const [status, setStatus] = useState<LocalStatus | null>(null);
  const [wizardOpen, setWizardOpen] = useState(false);
  const [steps, setSteps] = useState<Record<string, { status: StepStatus; detail?: string }>>({});
  const [log, setLog] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const logRef = useRef<HTMLPreElement | null>(null);

  const refresh = useCallback(() => {
    invoke<LocalStatus>("local_status")
      .then(setStatus)
      .catch(() => setStatus(null));
  }, []);

  useEffect(() => {
    refresh();
    const timer = setInterval(refresh, 10_000);
    return () => clearInterval(timer);
  }, [refresh]);

  useEffect(() => {
    const unlisteners: Promise<() => void>[] = [
      listen<{ step: string; status: StepStatus; detail?: string }>(
        "local-setup-progress",
        (e) => {
          setSteps((s) => ({
            ...s,
            [e.payload.step]: { status: e.payload.status, detail: e.payload.detail },
          }));
        }
      ),
      listen<string>("local-setup-log", (e) => {
        setLog((l) => [...l.slice(-499), e.payload]);
      }),
      listen<boolean>("local-setup-done", (e) => {
        refresh();
        if (e.payload) onInstancesChanged();
      }),
    ];
    return () => {
      for (const u of unlisteners) u.then((f) => f());
    };
  }, [refresh, onInstancesChanged]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [log]);

  async function handleSetup() {
    setWizardOpen(true);
    setSteps({});
    setLog([]);
    try {
      await invoke("local_setup");
    } catch (err) {
      setLog((l) => [...l, String(err)]);
    }
  }

  async function handleStartStop(action: "local_start" | "local_stop") {
    setBusy(true);
    try {
      await invoke(action);
    } catch (err) {
      alert(String(err));
    } finally {
      setBusy(false);
      refresh();
    }
  }

  if (!status || !status.supported) return null;

  const setupActive = status.setup_running || (wizardOpen && !status.installed);

  return (
    <section className="card local-panel">
      {!status.installed && !setupActive && (
        <div className="local-head">
          <div>
            <strong>Run NousViz on this computer</strong>
            <p className="muted">
              Downloads everything it needs (Python, Node, PostgreSQL, NousViz) into the
              Hub&apos;s own folder — nothing else on your system is touched.
            </p>
          </div>
          <button className="primary" onClick={handleSetup}>
            Set up local NousViz
          </button>
        </div>
      )}

      {(setupActive || (wizardOpen && status.installed)) && (
        <div className="local-wizard">
          <strong>Setting up NousViz on this computer</strong>
          <ul className="steps">
            {SETUP_STEPS.map((s) => {
              const st = steps[s.id]?.status ?? "pending";
              const mark =
                st === "ok" ? "✓" : st === "running" ? "…" : st === "error" ? "✗" : "○";
              return (
                <li key={s.id} className={`step ${st}`}>
                  <span className="step-mark">{mark}</span>
                  <span>{s.label}</span>
                  {steps[s.id]?.detail && (
                    <span className="muted step-detail">{steps[s.id].detail}</span>
                  )}
                </li>
              );
            })}
          </ul>
          <pre className="setup-log" ref={logRef}>
            {log.join("\n")}
          </pre>
          {status.installed && !status.setup_running && (
            <button onClick={() => setWizardOpen(false)}>Close</button>
          )}
        </div>
      )}

      {status.installed && !setupActive && !wizardOpen && (
        <div className="local-head">
          <div>
            <span className={`dot ${status.running ? "healthy" : "unreachable"}`} />
            <strong> This computer</strong>
            {status.version && <span className="badge"> v{status.version}</span>}
            <span className="muted"> {status.running ? "running" : "stopped"}</span>
          </div>
          <div className="instance-actions">
            {status.running ? (
              <button disabled={busy} onClick={() => handleStartStop("local_stop")}>
                Stop
              </button>
            ) : (
              <button className="primary" disabled={busy} onClick={() => handleStartStop("local_start")}>
                Start
              </button>
            )}
          </div>
        </div>
      )}
    </section>
  );
}

function App() {
  const [instances, setInstances] = useState<InstanceView[]>([]);
  const [health, setHealth] = useState<Record<string, Health>>({});
  const [addOpen, setAddOpen] = useState(false);
  const [newName, setNewName] = useState("");
  const [newUrl, setNewUrl] = useState("");
  const [addError, setAddError] = useState("");
  const [loginFor, setLoginFor] = useState<string | null>(null);
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [loginError, setLoginError] = useState("");
  const [busy, setBusy] = useState(false);

  const refreshHealth = useCallback((list: InstanceView[]) => {
    for (const inst of list) {
      setHealth((h) => ({ ...h, [inst.id]: { ...h[inst.id], state: h[inst.id]?.state ?? "checking" } }));
      invoke<Record<string, unknown>>("check_health", { url: inst.url })
        .then((res) => {
          const status = String(res.status ?? "");
          setHealth((h) => ({
            ...h,
            [inst.id]: {
              state: status === "healthy" ? "healthy" : "degraded",
              version: typeof res.version === "string" ? res.version : undefined,
            },
          }));
        })
        .catch((err) => {
          setHealth((h) => ({ ...h, [inst.id]: { state: "unreachable", detail: String(err) } }));
        });
    }
  }, []);

  const reload = useCallback(async () => {
    const list = await invoke<InstanceView[]>("list_instances");
    setInstances(list);
    refreshHealth(list);
  }, [refreshHealth]);

  useEffect(() => {
    reload();
  }, [reload]);

  useEffect(() => {
    const timer = setInterval(() => refreshHealth(instances), 30_000);
    return () => clearInterval(timer);
  }, [instances, refreshHealth]);

  async function handleAdd(e: React.FormEvent) {
    e.preventDefault();
    setAddError("");
    setBusy(true);
    try {
      await invoke("add_instance", { name: newName, url: newUrl });
      setNewName("");
      setNewUrl("");
      setAddOpen(false);
      await reload();
    } catch (err) {
      setAddError(String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleLogin(e: React.FormEvent, id: string) {
    e.preventDefault();
    setLoginError("");
    setBusy(true);
    try {
      await invoke("login_instance", { id, email, password });
      setLoginFor(null);
      setEmail("");
      setPassword("");
      await reload();
    } catch (err) {
      setLoginError(String(err));
    } finally {
      setBusy(false);
    }
  }

  async function handleRemove(inst: InstanceView) {
    if (!confirm(`Remove "${inst.name}"? Its saved login is deleted too.`)) return;
    await invoke("remove_instance", { id: inst.id });
    await reload();
  }

  async function handleLogout(id: string) {
    await invoke("logout_instance", { id });
    await reload();
  }

  async function handleOpen(id: string) {
    try {
      await invoke("open_instance", { id });
    } catch (err) {
      alert(`Could not open window: ${err}`);
    }
  }

  return (
    <main className="shell">
      <header className="topbar">
        <h1>NousViz Hub</h1>
        <button className="primary" onClick={() => setAddOpen((v) => !v)}>
          {addOpen ? "Cancel" : "+ Add instance"}
        </button>
      </header>

      <LocalPanel onInstancesChanged={reload} />

      {addOpen && (
        <form className="card add-form" onSubmit={handleAdd}>
          <input
            placeholder="Name (e.g. Production)"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            autoFocus
          />
          <input
            placeholder="URL (e.g. https://nousviz.example.com)"
            value={newUrl}
            onChange={(e) => setNewUrl(e.target.value)}
          />
          {addError && <p className="error">{addError}</p>}
          <button className="primary" type="submit" disabled={busy}>
            Add
          </button>
        </form>
      )}

      {instances.length === 0 && !addOpen && (
        <div className="empty">
          <p>No instances yet.</p>
          <p className="muted">Add a NousViz server by its URL — a droplet, or a local install.</p>
        </div>
      )}

      <ul className="instances">
        {instances.map((inst) => {
          const h = health[inst.id] ?? { state: "checking" as const };
          return (
            <li key={inst.id} className="card instance">
              <div className="instance-head">
                <span className={`dot ${h.state}`} title={h.detail ?? h.state} />
                <div className="instance-id">
                  <strong>{inst.name}</strong>
                  <span className="muted">{inst.url}</span>
                </div>
                <div className="instance-meta">
                  {h.version && <span className="badge">v{h.version}</span>}
                  {h.state === "unreachable" && <span className="badge bad">offline</span>}
                </div>
              </div>

              <div className="instance-actions">
                <button className="primary" onClick={() => handleOpen(inst.id)}>
                  Open
                </button>
                {inst.has_token ? (
                  <>
                    <span className="muted session">
                      {inst.email ?? "signed in"}
                      {inst.token_in_fallback && " ⚠ stored unencrypted"}
                    </span>
                    <button onClick={() => handleLogout(inst.id)}>Sign out</button>
                  </>
                ) : (
                  <button
                    onClick={() => {
                      setLoginFor(loginFor === inst.id ? null : inst.id);
                      setEmail(inst.email ?? "");
                      setPassword("");
                      setLoginError("");
                    }}
                  >
                    Sign in
                  </button>
                )}
                <button className="danger" onClick={() => handleRemove(inst)}>
                  Remove
                </button>
              </div>

              {loginFor === inst.id && (
                <form className="login-form" onSubmit={(e) => handleLogin(e, inst.id)}>
                  <input
                    type="email"
                    placeholder="Email"
                    value={email}
                    onChange={(e) => setEmail(e.target.value)}
                    autoFocus
                  />
                  <input
                    type="password"
                    placeholder="Password"
                    value={password}
                    onChange={(e) => setPassword(e.target.value)}
                  />
                  {loginError && <p className="error">{loginError}</p>}
                  <button className="primary" type="submit" disabled={busy}>
                    Sign in
                  </button>
                </form>
              )}
            </li>
          );
        })}
      </ul>
    </main>
  );
}

export default App;
