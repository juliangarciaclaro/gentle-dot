import type { AgentState } from "@gentle-dot/protocol";
import { useEffect, useRef } from "react";
import { Rose, type RoseMood } from "../rose/Rose.tsx";
import { computerName } from "../shortcut.ts";

const LABELS: Record<AgentState, string> = {
	starting: "starting",
	idle: "ready",
	thinking: "thinking",
	working: "working",
	needs_you: "needs your answer",
	restarting: "restarting",
	error: "unavailable",
};

const MOODS: Record<AgentState, RoseMood> = {
	starting: "restarting",
	idle: "idle",
	thinking: "thinking",
	working: "working",
	needs_you: "needs",
	restarting: "restarting",
	error: "offline",
};

const DRAG_THRESHOLD_PX = 3;

/**
 * Where the Dot is a layer surface (S9) the app drags it itself from GTK, because the web view
 * stops getting pointer events once the pointer leaves the Dot. The UI only keeps the end of a drag
 * from reading as a click.
 */
export interface LayerDrag {
	/** Calls `listener` when the app starts dragging the Dot; returns the way to stop listening. */
	onStart: (listener: () => void) => () => void;
}

interface Press {
	x: number;
	y: number;
	dragging: boolean;
}

interface DotSurfaceProps {
	agentState: AgentState;
	connected: boolean;
	/** The assistant controls the computer (S24.6); it shows over every other state. */
	inControl?: boolean;
	/** Yolo mode is on (S24.9): risky actions run without asking, so the rose says so. */
	yolo?: boolean;
	toggle: () => void;
	startDrag: () => void;
	/** Set where the app drags the Dot itself instead of `startDrag`. */
	layerDrag?: LayerDrag;
}

/** The collapsed, always-on-top rose in its black circle. A click opens the panel; a drag moves the window. */
export function DotSurface({
	agentState,
	connected,
	inControl = false,
	yolo = false,
	toggle,
	startDrag,
	layerDrag,
}: DotSurfaceProps) {
	const press = useRef<Press | undefined>(undefined);
	const state = connected ? agentState : "restarting";
	const label = inControl ? `controlling your ${computerName()}` : LABELS[state];

	// The app may start the drag before this window sees the pointer move past the threshold.
	useEffect(
		() =>
			layerDrag?.onStart(() => {
				if (press.current) press.current.dragging = true;
			}),
		[layerDrag],
	);

	return (
		<button
			type="button"
			className={`dot dot-${state}${inControl ? " dot-in_control" : ""}${yolo ? " dot-yolo" : ""}`}
			aria-label={`Gentle Dot: ${label}${yolo ? ", yolo mode on" : ""}`}
			onPointerDown={(event) => {
				if (event.button !== 0) return;
				press.current = { x: event.clientX, y: event.clientY, dragging: false };
			}}
			onPointerMove={(event) => {
				const p = press.current;
				if (!p || p.dragging) return;
				if (Math.hypot(event.clientX - p.x, event.clientY - p.y) > DRAG_THRESHOLD_PX) {
					p.dragging = true;
					if (!layerDrag) startDrag();
				}
			}}
			onPointerUp={() => {
				const p = press.current;
				press.current = undefined;
				if (p && !p.dragging) toggle();
			}}
			onPointerCancel={() => {
				press.current = undefined;
			}}
			onKeyDown={(event) => {
				if (event.key === "Enter" || event.key === " ") {
					event.preventDefault();
					toggle();
				}
			}}
		>
			<span className="dot-disc" aria-hidden="true">
				<Rose mood={inControl ? "control" : MOODS[state]} />
			</span>
			{state === "needs_you" ? (
				<span className="dot-badge" aria-hidden="true">
					!
				</span>
			) : null}
		</button>
	);
}
