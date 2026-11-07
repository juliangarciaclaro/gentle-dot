import type { AgentState, ClientMessage } from "@gentle-dot/protocol";
import { invoke, isTauri } from "@tauri-apps/api/core";
import type { ConnectionInfo } from "./client.ts";

/** Bridge to the Tauri shell (docs/design.md §9). Every call is a no-op in the browser. */
export const inDesktop = (): boolean => isTauri();

export function connectionInfo(): Promise<ConnectionInfo> {
	return invoke<ConnectionInfo>("connection_info");
}

/**
 * A connector change from the panel (S25.2): the app sends it to the daemon over its private
 * channel, on behalf of this window (`clientId`, from `ready`). Rejects with the app's reason.
 */
export function connectorCommand(clientId: string, message: ClientMessage): Promise<void> {
	return invoke<void>("connector_command", { clientId, message });
}

export function togglePanel(): void {
	void invoke("toggle_panel");
}

export function hidePanel(): void {
	if (inDesktop()) void invoke("hide_panel");
}

export function setDotState(state: AgentState): void {
	if (inDesktop()) void invoke("set_dot_state", { state });
}

/** Opens a web page (sign-in) in the user's browser. Only http and https. */
export function openUrl(url: string): void {
	if (!/^https?:\/\//i.test(url)) return;
	if (inDesktop()) {
		void import("@tauri-apps/plugin-opener").then(({ openUrl: open }) => open(url));
		return;
	}
	window.open(url, "_blank", "noopener,noreferrer");
}

/**
 * Whether the app moves the Dot itself (Wayland layer surface, S9), which `startDragging` cannot:
 * it follows the pointer from GTK and the UI only keeps the end of a drag from being a click.
 */
export function dotDragsByLayer(): Promise<boolean> {
	return invoke<boolean>("dot_drag_mode");
}

/** Calls `listener` when the app starts dragging the Dot itself (`dotDragsByLayer`). */
export function onDotDragStarted(listener: () => void): () => void {
	const unlisten = onDesktopEvent("dot://dot-drag-started", listener);
	return () => void unlisten.then((off) => off());
}

export async function startDragging(): Promise<void> {
	const { getCurrentWindow } = await import("@tauri-apps/api/window");
	await getCurrentWindow().startDragging();
}

/**
 * `dot://assistant-ready`: the app's assistant just started answering (at launch or after Restart
 * assistant), so a window it refused can ask `connection_info` again (S35.2).
 */
export async function onDesktopEvent(
	name: "dot://new-conversation" | "dot://panel-shown" | "dot://assistant-ready" | "dot://dot-drag-started",
	handler: () => void,
) {
	if (!inDesktop()) return () => {};
	const { listen } = await import("@tauri-apps/api/event");
	return listen(name, handler);
}

/**
 * The clipboard's image as a PNG file, read by the app; undefined when it holds none. Only the
 * Linux app reads it: WebKitGTK gives a paste no image data, while the other web views do.
 */
export async function clipboardImage(): Promise<File | undefined> {
	const png = await invoke<string | null>("clipboard_image");
	if (!png) return undefined;
	const bytes = Uint8Array.from(atob(png), (c) => c.charCodeAt(0));
	return new File([bytes], "pasted-image.png", { type: "image/png" });
}
