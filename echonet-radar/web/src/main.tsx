import ReactDOM from "react-dom/client";
import "@cloudflare/kumo/styles";
import "./index.css";
import { App } from "./App";

const colorScheme = window.matchMedia("(prefers-color-scheme: dark)");
const applyMode = () => {
  document.documentElement.dataset.mode = colorScheme.matches ? "dark" : "light";
};
applyMode();
colorScheme.addEventListener("change", applyMode);

ReactDOM.createRoot(document.getElementById("root")!).render(<App />);