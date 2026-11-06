import type { AgentState } from "@gentle-dot/protocol";
import { useRef } from "react";
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

/** Moving the Dot ourselves where it is a layer surface (S9) and `startDragging` cannot. */
export interface LayerDrag {
	/** Moves the Dot by the pointer's movement since the press; resolves once it has moved. */
	by: (dx: number, dy: number) => Promise<void>;
	/** The pointer was released: the Dot snaps to the nearest edge. */
	end: () => void;
}

interface Press {
	x: number;
	y: number;
	dragging: boolean;
	/** The latest offset since the press, not yet sent. */
	offset?: { dx: number; dy: number };
	frame?: number;
	/** An offset is on its way; the next waits so the Dot has moved under the pointer first. */
	busy: boolean;
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
	/** Set where the app drags the Dot from here instead of `startDrag`. */
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

	/** At most one message per animation frame, and none while the last is still moving the Dot. */
	const sendOffset = (p: Press) => {
		p.frame = undefined;
		if (!p.offset || !layerDrag) return;
		if (p.busy) {
			p.frame = requestAnimationFrame(() => sendOffset(p));
			return;
		}
		const { dx, dy } = p.offset;
		p.offset = undefined;
		p.busy = true;
		void layerDrag.by(dx, dy).finally(() => {
			p.busy = false;
		});
	};
	const finish = () => {
		const p = press.current;
		press.current = undefined;
		if (p?.frame !== undefined) cancelAnimationFrame(p.frame);
		if (p?.dragging && layerDrag) layerDrag.end();
		return p;
	};

	return (
		<button
			type="button"
			className={`dot dot-${state}${inControl ? " dot-in_control" : ""}${yolo ? " dot-yolo" : ""}`}
			aria-label={`Gentle Dot: ${label}${yolo ? ", yolo mode on" : ""}`}
			onPointerDown={(event) => {
				if (event.button !== 0) return;
				press.current = { x: event.clientX, y: event.clientY, dragging: false, busy: false };
			}}
			onPointerMove={(event) => {
				const p = press.current;
				if (!p) return;
				if (p.dragging) {
					if (!layerDrag) return;
					p.offset = { dx: event.clientX - p.x, dy: event.clientY - p.y };
					p.frame ??= requestAnimationFrame(() => sendOffset(p));
					return;
				}
				if (Math.hypot(event.clientX - p.x, event.clientY - p.y) > DRAG_THRESHOLD_PX) {
					p.dragging = true;
					if (layerDrag) {
						// Keep the pointer events coming while the Dot moves under the pointer.
						event.currentTarget.setPointerCapture?.(event.pointerId);
						p.offset = { dx: event.clientX - p.x, dy: event.clientY - p.y };
						p.frame = requestAnimationFrame(() => sendOffset(p));
					} else {
						startDrag();
					}
				}
			}}
			onPointerUp={() => {
				const p = finish();
				if (p && !p.dragging) toggle();
			}}
			onPointerCancel={() => void finish()}
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
