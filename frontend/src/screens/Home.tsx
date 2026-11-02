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
        <h1>Sharescreen</h1>
        <button className="ghost" onClick={onOpenSettings}>
          Settings
        </button>
      </header>

      <section className="card">
        <h2>Server</h2>
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
            <button className="primary" onClick={onConnect}>
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

      <section className="card">
        <h2>Room</h2>
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
          <h2>Participants</h2>
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
