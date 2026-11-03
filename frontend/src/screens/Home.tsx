import { useState } from "react";

import type { Config, Participant } from "../lib/types";

interface HomeProps {
  config: Config;
  connected: boolean;
  participants: Participant[];
  onConnect: () => void;
  onDisconnect: () => void;
  onCreateRoom: () => void;
  onJoinRoom: (roomId: string) => void;
  onOpenSettings: () => void;
  onSaveConfig: (config: Config) => void;
}

export default function Home({
  config,
  connected,
  participants,
  onConnect,
  onDisconnect,
  onCreateRoom,
  onJoinRoom,
  onOpenSettings,
  onSaveConfig,
}: HomeProps) {
  const [roomId, setRoomId] = useState("");
  const [displayName, setDisplayName] = useState(config.state.display_name);
  const nameNeedsSave = displayName.trim() !== config.state.display_name;

  const saveDisplayName = () => {
    const name = displayName.trim();
    if (!name || !nameNeedsSave) return;
    onSaveConfig({ ...config, state: { ...config.state, display_name: name } });
  };

  const setHost = (host: string) =>
    onSaveConfig({
      ...config,
      network: { ...config.network, signaling_host: host },
    });
  const setPort = (port: number) =>
    onSaveConfig({
      ...config,
      network: { ...config.network, signaling_port: port },
    });
  const setSecure = (secure: boolean) =>
    onSaveConfig({
      ...config,
      network: { ...config.network, signaling_secure: secure },
    });

  const recent = config.state.recent_rooms;

  return (
    <div className="home">
      <header className="home-header">
        <div className="home-brand">
          <span className="brand-mark" aria-hidden="true">
            S
          </span>
          <div>
            <p className="eyebrow">SHARESCREEN</p>
            <h1>Share your screen, together.</h1>
          </div>
        </div>
        <button className="ghost" onClick={onOpenSettings}>
          Settings
        </button>
      </header>

      <section className="profile-card">
        <span className="avatar profile-avatar">{initials(displayName)}</span>
        <label className="grow">
          <span>Your display name</span>
          <input
            value={displayName}
            maxLength={32}
            onChange={(e) => setDisplayName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") saveDisplayName();
            }}
            placeholder="Name others will see"
          />
        </label>
        <button
          className={nameNeedsSave ? "secondary" : "ghost"}
          disabled={!displayName.trim() || !nameNeedsSave}
          onClick={saveDisplayName}
        >
          Save name
        </button>
        <span className="small muted profile-hint">
          Used when you connect next time.
        </span>
      </section>

      <section className="card">
        <div className="section-heading">
          <div>
            <p className="eyebrow">CONNECTION</p>
            <h2>Signaling server</h2>
          </div>
          <span className={connected ? "status-pill online" : "status-pill"}>
            <span className="status-dot" />
            {connected ? "Connected" : "Not connected"}
          </span>
        </div>
        <div className="row">
          <label className="grow">
            <span>Address</span>
            <input
              value={config.network.signaling_host}
              onChange={(e) => setHost(e.target.value)}
              placeholder="127.0.0.1"
              spellCheck={false}
            />
          </label>
          <label>
            <span>Port</span>
            <input
              type="number"
              value={config.network.signaling_port}
              onChange={(e) => setPort(Number(e.target.value))}
            />
          </label>
        </div>
        <div className="row">
          <label className="checkbox">
            <input
              type="checkbox"
              checked={config.network.signaling_secure}
              onChange={(e) => setSecure(e.target.checked)}
            />
            <span>Use secure WebSocket (wss://)</span>
          </label>
          <div className="grow" />
          {connected ? (
            <button className="danger" onClick={onDisconnect}>
              Disconnect
            </button>
          ) : (
            <button
              className="primary"
              disabled={!config.state.display_name.trim() || nameNeedsSave}
              onClick={onConnect}
            >
              Connect
            </button>
          )}
        </div>
        <p className="muted">
          {connected
            ? `Connected to ${config.network.signaling_secure ? "wss" : "ws"}://${config.network.signaling_host}:${config.network.signaling_port}`
            : "Not connected to a signaling server."}
        </p>
      </section>

      <section className="card room-card">
        <div className="section-heading">
          <div>
            <p className="eyebrow">GET TOGETHER</p>
            <h2>Join or create a room</h2>
          </div>
        </div>
        <div className="row">
          <label className="grow">
            <span>Room ID</span>
            <input
              value={roomId}
              onChange={(e) => setRoomId(e.target.value.trim())}
              placeholder="paste a room id or invite code"
              spellCheck={false}
            />
          </label>
          <button
            className="primary"
            disabled={!connected || roomId.length === 0}
            onClick={() => onJoinRoom(roomId)}
          >
            Join
          </button>
          <button className="secondary" disabled={!connected} onClick={onCreateRoom}>
            Create room
          </button>
        </div>
        {recent.length > 0 && (
          <>
            <h3>Recent rooms</h3>
            <ul className="recent">
              {recent.map((id) => (
                <li key={id}>
                  <button className="link" onClick={() => onJoinRoom(id)}>
                    {id}
                  </button>
                </li>
              ))}
            </ul>
          </>
        )}
      </section>

      {participants.length > 0 && (
        <section className="card">
          <div className="section-heading">
            <h2>Participants</h2>
            <span className="status-pill">{participants.length} online</span>
          </div>
          <ul className="participants">
            {participants.map((p) => (
              <li key={p.id}>
                <span className="avatar">{initials(p.name)}</span>
                <span className="grow">{p.name}</span>
                <span className="muted">{p.connection}</span>
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[1][0]).toUpperCase();
}
