import { useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

/// Balão que diz qual perfil está em uso. Vive numa janela própria, transparente
/// e sem foco; quem manda mostrar/esconder é o Rust.
export default function Toast() {
  const [text, setText] = useState("");

  useEffect(() => {
    for (const el of [document.documentElement, document.body]) {
      el.style.background = "transparent";
      el.style.margin = "0";
      el.style.padding = "0";
      el.style.overflow = "hidden";
    }
    const un = listen<string>("toast", (e) => setText(e.payload));
    return () => {
      un.then((f) => f());
    };
  }, []);

  return (
    <div
      style={{
        display: "flex",
        alignItems: "center",
        justifyContent: "center",
        gap: 8,
        height: 44,
        font: "500 13px/1 system-ui, -apple-system, Segoe UI, sans-serif",
        color: "#fff",
      }}
    >
      <div
        style={{
          display: "flex",
          alignItems: "center",
          gap: 8,
          maxWidth: "100%",
          padding: "8px 14px",
          borderRadius: 999,
          background: "rgba(20, 20, 22, 0.92)",
          border: "1px solid rgba(255, 255, 255, 0.14)",
          boxShadow: "0 6px 20px rgba(0, 0, 0, 0.45)",
        }}
      >
        <svg width="15" height="9" viewBox="0 0 28 16" fill="currentColor" aria-hidden>
          <rect x="1" y="7.4" width="26" height="1.2" rx="0.6" />
          <rect x="9.87" y="3" width="2.4" height="10" rx="1.2" />
          <rect x="12.8" y="1.5" width="2.4" height="13" rx="1.2" />
          <rect x="15.73" y="3" width="2.4" height="10" rx="1.2" />
        </svg>
        <span
          style={{
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {text}
        </span>
      </div>
    </div>
  );
}
