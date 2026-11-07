import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
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

      {addOpen && (
        <form className="card add-form" onSubmit={handleAdd}>
          <input
            placeholder="Name (e.g. CS StatsDrone)"
            value={newName}
            onChange={(e) => setNewName(e.target.value)}
            autoFocus
          />
          <input
            placeholder="URL (e.g. https://cs.statsdroneinc.com)"
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
