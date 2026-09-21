import { RouterProvider, createBrowserRouter } from "react-router-dom";
import { loadSession } from "./api";
import { setAuthed } from "./auth";
import { routes } from "./routes";
import { ToastProvider } from "./ui";

// Built once at module scope (react-router's documented pattern). Rebuilding
// the router inside App's render — what this file used to do — remounted the
// whole tree, re-running every page's effects, on each auth flip.
setAuthed(loadSession());
const router = createBrowserRouter(routes, { basename: "/admin" });

export default function App() {
  return (
    <ToastProvider>
      <RouterProvider router={router} />
    </ToastProvider>
  );
}
