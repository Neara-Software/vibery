import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter } from "react-router-dom";
import App from "./App";
import "./style.css";

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <BrowserRouter>
      <App />
    </BrowserRouter>
  </StrictMode>,
);

// Warm the markdown renderer chunk once the first screen is up, so opening an entity never waits on it.
const warm = () => import("./Markdown");
("requestIdleCallback" in window ? requestIdleCallback : setTimeout)(warm as () => void);
