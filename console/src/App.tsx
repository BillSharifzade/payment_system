import { RouterProvider, createBrowserRouter } from "react-router-dom";
import { purgeLegacyStorage } from "./api";
import { routes } from "./routes";
import { ToastProvider } from "./ui";

// Sessions live in memory only, so every page load starts signed out. Older
// builds kept tokens (and a shared pending-deposit record) in web storage;
// wipe them so an upgraded browser stops carrying them.
purgeLegacyStorage();

// Built once at module scope (react-router's documented pattern). Rebuilding
// the router inside App's render — what this file used to do — remounted the
// whole tree, re-running every page's effects, on each auth flip.
const router = createBrowserRouter(routes, { basename: "/admin" });

export default function App() {
  return (
    <ToastProvider>
      <RouterProvider router={router} />
    </ToastProvider>
  );
}
