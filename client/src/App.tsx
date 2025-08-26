import { useCallback, useEffect, useRef, useState } from "react";

export const App = () => {
  const [status, setStatus] = useState("disconnected");
  // const [messages, setMessages] = useState([]);
  // const [input, setInput] = useState("");
  const wsr = useRef<WebSocket | null>(null);

  useEffect(() => {
    const ws = new WebSocket("ws://localhost:8000/ws");
    ws.addEventListener("open", () => {
      setStatus("connected");
      wsr.current = ws;
    });

    ws.addEventListener("message", (event) => {
      console.info(`[ws] message: ${event.data}`);
    });

    ws.addEventListener("close", () => {
      setStatus("closed");
    });

    ws.addEventListener("error", () => {
      setStatus("error");
    });

    // アンマウント時に接続を閉じる
    return () => {
      ws.close();
    };
  }, []);

  const sendMessage = useCallback((message: string) => {
    wsr.current?.send(message);
  }, []);

  return (
    <div>
      App connected: {status} <button onClick={() => sendMessage("ping")}>send ping</button>
    </div>
  );
};
