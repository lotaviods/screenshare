import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Pip from "./screens/Pip";
import "./styles.css";

// The picture-in-picture window is the same bundle, opened with `?pip=1`.
const isPip = new URLSearchParams(window.location.search).has("pip");

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>{isPip ? <Pip /> : <App />}</React.StrictMode>,
);
