import { useCallback, useEffect, useState } from "react";
import type { UnlistenFn } from "@tauri-apps/api/event";

import { api, errorMessage, onCallEvent } from "./lib/bridge";
import type { CallEvent, Config, Participant } from "./lib/types";
import Call from "./screens/Call";
import Home from "./screens/Home";
import Settings from "./screens/Settings";

type Screen = "home" | "call" | "settings";

/** Insert or replace a participant in the roster. */
function upsert(prev: Participant[], participant: Participant): Participant[] {
  const index = prev.findIndex((p) => p.id === participant.id);
  if (index === -1) return [...prev, participant];
  const next = [...prev];
  next[index] = participant;
  return next;
}

export default function App() {
  const [config, setConfig] = useState<Config | null>(null);
  const [screen, setScreen] = useState<Screen>("home");
  const [connected, setConnected] = useState(false);
  const [roomId, setRoomId] = useState<string | null>(null);
  const [participants, setParticipants] = useState<Participant[]>([]);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    api
      .getConfig()
      .then(setConfig)
      .catch((e) => setError(errorMessage(e)));
  }, []);

  useEffect(() => {
    let unlisten: UnlistenFn | undefined;
    let disposed = false;

    void onCallEvent((event: CallEvent) => {
      switch (event.type) {
        case "room_created":
        case "room_joined":
          setRoomId(event.room_id);
          setScreen("call");
          break;
        case "room_deleted":
          setRoomId(null);
          setParticipants([]);
          setScreen("home");
          break;
        case "participant_joined":
          setParticipants((prev) => upsert(prev, event.participant));
          break;
        case "participant_left":
          setParticipants((prev) => prev.filter((p) => p.id !== event.id));
          break;
        case "participant_updated":
          setParticipants((prev) => upsert(prev, event.participant));
          break;
        case "error":
          setError(event.message);
          break;
      }
    }).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });

    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  const saveConfig = useCallback(async (next: Config) => {
    setConfig(next);
    try {
      await api.updateConfig(next);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  const connect = useCallback(async () => {
    try {
      await api.connectServer();
      setConnected(true);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  const disconnect = useCallback(async () => {
    try {
      await api.disconnectServer();
    } catch (e) {
      setError(errorMessage(e));
    }
    setConnected(false);
    setRoomId(null);
    setParticipants([]);
    setScreen("home");
  }, []);

  const createRoom = useCallback(async () => {
    try {
      await api.createRoom();
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  const joinRoom = useCallback(async (id: string) => {
    try {
      await api.joinRoom(id);
      setError(null);
    } catch (e) {
      setError(errorMessage(e));
    }
  }, []);

  const leaveRoom = useCallback(async () => {
    try {
      await api.leaveRoom();
    } catch (e) {
      setError(errorMessage(e));
    }
    setRoomId(null);
    setParticipants([]);
    setScreen("home");
  }, []);

  const deleteRoom = useCallback(async () => {
    try {
      await api.deleteRoom();
    } catch (e) {
      setError(errorMessage(e));
    }
    setRoomId(null);
    setParticipants([]);
    setScreen("home");
  }, []);

  if (!config) {
    return (
      <div className="app">
        <p className="muted">{error ?? "Loading configuration…"}</p>
      </div>
    );
  }

  return (
    <div className="app">
      {error && (
        <div className="banner error" role="alert">
          {error}
          <button className="link" onClick={() => setError(null)}>
            dismiss
          </button>
        </div>
      )}

      {screen === "call" && roomId ? (
        <Call
          config={config}
          roomId={roomId}
          participants={participants}
          onLeave={leaveRoom}
          onDeleteRoom={deleteRoom}
          onOpenSettings={() => setScreen("settings")}
          onSaveConfig={saveConfig}
        />
      ) : screen === "settings" ? (
        <Settings
          config={config}
          onBack={() => setScreen(roomId ? "call" : "home")}
          onSave={saveConfig}
        />
      ) : (
        <Home
          config={config}
          connected={connected}
          participants={participants}
          onConnect={connect}
          onDisconnect={disconnect}
          onCreateRoom={createRoom}
          onJoinRoom={joinRoom}
          onOpenSettings={() => setScreen("settings")}
          onSaveConfig={saveConfig}
        />
      )}
    </div>
  );
}
