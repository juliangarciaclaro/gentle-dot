//! The pointer path planner for the uinput device (S2). An absolute uinput device maps its
//! range onto the output currently under the pointer (probe L4), and relative motion crosses
//! outputs, so a move is planned as: relative pushes to a known layout corner, then for each
//! output on the path an absolute move to the edge facing the next output plus a small relative
//! nudge across it, then an absolute move to the target inside its output.
//!
//! Pure: `simulate` is a simple model of the compositor that the tests (and `plan`) use to
//! check where the pointer ends up.

use std::collections::VecDeque;

use crate::computer::coords::Point;

/// An output's logical position and size in the global layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// One pointer event for the uinput device.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Relative motion in logical pixels.
    Relative { dx: i32, dy: i32 },
    /// Absolute position as a fraction 0..=1 of the output currently under the pointer.
    Absolute { x: f64, y: f64 },
}

/// Size of each push step toward the layout corner, and how many: together far wider than any
/// layout, in moderate steps so the pointer walks across adjacent outputs.
const PUSH_STEP: i32 = 400;
const PUSH_STEPS: usize = 20;
/// How far past an edge a hop nudges the pointer.
const NUDGE: i32 = 40;
/// How far the first motion of a drag goes: past the drag threshold of common toolkits (GTK: 8).
pub const DRAG_LEAD: f64 = 16.0;
/// The motions spread over the rest of a drag, after the lead.
const DRAG_STEPS: usize = 8;
/// The wait after a drag's first motion: apps start their drag only once they see it, and only
/// motions after that reach the drop target (L70 of odd/tasks/linux-parity.md).
pub const DRAG_START: std::time::Duration = std::time::Duration::from_millis(100);

impl Rect {
    fn right(&self) -> i32 {
        self.x + self.width
    }

    fn bottom(&self) -> i32 {
        self.y + self.height
    }

    fn contains(&self, p: Point) -> bool {
        p.x >= self.x as f64 && p.x < self.right() as f64 && p.y >= self.y as f64 && p.y < self.bottom() as f64
    }

    /// The pixel of this rect closest to `p`.
    fn clamp(&self, p: Point) -> Point {
        Point {
            x: p.x.clamp(self.x as f64, (self.right() - 1) as f64),
            y: p.y.clamp(self.y as f64, (self.bottom() - 1) as f64),
        }
    }

    /// The fraction of this rect's pixel range that `p` sits at, clamped to 0..=1.
    fn fraction(&self, p: Point) -> (f64, f64) {
        let along = |v: f64, origin: i32, size: i32| ((v - origin as f64) / (size - 1).max(1) as f64).clamp(0.0, 1.0);
        (along(p.x, self.x, self.width), along(p.y, self.y, self.height))
    }
}

/// The output under `p`, else the one nearest to it.
fn output_at(outputs: &[Rect], p: Point) -> Option<usize> {
    outputs
        .iter()
        .position(|out| out.contains(p))
        .or_else(|| nearest(outputs, p).map(|(index, _)| index))
}

fn nearest(outputs: &[Rect], p: Point) -> Option<(usize, Point)> {
    let distance = |q: Point| (q.x - p.x).powi(2) + (q.y - p.y).powi(2);
    outputs
        .iter()
        .enumerate()
        .map(|(index, out)| (index, out.clamp(p)))
        .min_by(|(_, a), (_, b)| distance(*a).total_cmp(&distance(*b)))
}

/// Where the pointer ends after `steps` from `start`: relative motion is clamped to the layout
/// (the nearest pixel of any output), absolute motion lands at the fraction of the output under
/// the pointer.
pub fn simulate(outputs: &[Rect], start: Point, steps: &[Step]) -> Point {
    let mut at = start;
    for step in steps {
        at = match *step {
            Step::Relative { dx, dy } => {
                let moved = Point { x: at.x + dx as f64, y: at.y + dy as f64 };
                nearest(outputs, moved).map_or(moved, |(_, clamped)| clamped)
            }
            Step::Absolute { x, y } => match output_at(outputs, at) {
                Some(index) => {
                    let out = outputs[index];
                    Point {
                        x: out.x as f64 + x * (out.width - 1) as f64,
                        y: out.y as f64 + y * (out.height - 1) as f64,
                    }
                }
                None => at,
            },
        };
    }
    at
}

/// The pushes toward the top-left of the layout: left, up, then left and up again so layouts
/// whose rows and columns are staggered settle at the same corner from any start.
fn pushes() -> Vec<Step> {
    let left = Step::Relative { dx: -PUSH_STEP, dy: 0 };
    let up = Step::Relative { dx: 0, dy: -PUSH_STEP };
    [left, up, left, up].into_iter().flat_map(|step| std::iter::repeat_n(step, PUSH_STEPS)).collect()
}

/// Which way a hop crosses a shared edge, and the span of the edge on the other axis.
#[derive(Clone, Copy)]
enum Edge {
    Right(i32, i32),
    Left(i32, i32),
    Down(i32, i32),
    Up(i32, i32),
}

fn overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> Option<(i32, i32)> {
    let (start, end) = (a_start.max(b_start), a_end.min(b_end));
    (start < end).then_some((start, end))
}

/// The edge `from` shares with `to`, if they are adjacent along a segment.
fn shared_edge(from: Rect, to: Rect) -> Option<Edge> {
    if from.right() == to.x {
        overlap(from.y, from.bottom(), to.y, to.bottom()).map(|(a, b)| Edge::Right(a, b))
    } else if from.x == to.right() {
        overlap(from.y, from.bottom(), to.y, to.bottom()).map(|(a, b)| Edge::Left(a, b))
    } else if from.bottom() == to.y {
        overlap(from.x, from.right(), to.x, to.right()).map(|(a, b)| Edge::Down(a, b))
    } else if from.y == to.bottom() {
        overlap(from.x, from.right(), to.x, to.right()).map(|(a, b)| Edge::Up(a, b))
    } else {
        None
    }
}

/// Output indexes from `from` to `to` through adjacent outputs, fewest hops.
fn route(outputs: &[Rect], from: usize, to: usize) -> Option<Vec<usize>> {
    let mut parent = vec![None; outputs.len()];
    let mut seen = vec![false; outputs.len()];
    seen[from] = true;
    let mut queue = VecDeque::from([from]);
    while let Some(current) = queue.pop_front() {
        if current == to {
            let mut path = vec![to];
            while let Some(previous) = parent[*path.last().expect("path is never empty")] {
                path.push(previous);
            }
            path.reverse();
            return Some(path);
        }
        for next in 0..outputs.len() {
            if !seen[next] && shared_edge(outputs[current], outputs[next]).is_some() {
                seen[next] = true;
                parent[next] = Some(current);
                queue.push_back(next);
            }
        }
    }
    None
}

/// The pointer events that bring the pointer from anywhere to `target` (logical global
/// pixels). Errors when the target is in no output or cannot be reached through adjacent ones.
pub fn plan(outputs: &[Rect], target: Point) -> Result<Vec<Step>, String> {
    let goal = goal_output(outputs, target)?;
    let mut steps = pushes();
    // The pushes end in the layout's top-left corner wherever the pointer began, so the
    // simulation starts from any output.
    let corner = simulate(outputs, centre(outputs[0]), &steps);
    let current = output_at(outputs, corner).expect("outputs is not empty");
    steps.extend(hops(outputs, current, goal, target)?);
    Ok(steps)
}

/// The events that bring a pointer known to be at `start` to `target`, without the pushes to the
/// corner: a drag must not drag its item across the layout. Errors when either point is in no
/// output or the outputs are not connected.
pub fn plan_from(outputs: &[Rect], start: Point, target: Point) -> Result<Vec<Step>, String> {
    let goal = goal_output(outputs, target)?;
    let current = outputs
        .iter()
        .position(|out| out.contains(start))
        .ok_or_else(|| format!("point ({:.0}, {:.0}) is outside every output", start.x, start.y))?;
    hops(outputs, current, goal, target)
}

fn goal_output(outputs: &[Rect], target: Point) -> Result<usize, String> {
    outputs
        .iter()
        .position(|out| out.contains(target))
        .ok_or_else(|| format!("point ({:.0}, {:.0}) is outside every output", target.x, target.y))
}

/// The hops from output `current` to the one holding `target`, then the move inside it.
fn hops(outputs: &[Rect], mut current: usize, goal: usize, target: Point) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let path = route(outputs, current, goal)
        .ok_or_else(|| "the output holding the target is not connected to the pointer's output".to_owned())?;
    for next in path.into_iter().skip(1) {
        let from = outputs[current];
        let to = outputs[next];
        let edge = shared_edge(from, to).expect("route hops are adjacent");
        let (absolute, nudge) = match edge {
            Edge::Right(a, b) => ((from.right() - 1, (a + b) / 2), (NUDGE.min(to.width - 1), 0)),
            Edge::Left(a, b) => ((from.x, (a + b) / 2), (-NUDGE.min(to.width - 1), 0)),
            Edge::Down(a, b) => (((a + b) / 2, from.bottom() - 1), (0, NUDGE.min(to.height - 1))),
            Edge::Up(a, b) => (((a + b) / 2, from.y), (0, -NUDGE.min(to.height - 1))),
        };
        let (x, y) = from.fraction(Point { x: absolute.0 as f64, y: absolute.1 as f64 });
        steps.push(Step::Absolute { x, y });
        steps.push(Step::Relative { dx: nudge.0, dy: nudge.1 });
        current = next;
    }
    let (x, y) = outputs[goal].fraction(target);
    steps.push(Step::Absolute { x, y });
    Ok(steps)
}

/// The points a drag passes through after the press: a short first motion that makes the app
/// start its drag, then even steps to `to`. The callers wait `DRAG_START` after the first point.
/// A drag shorter than the lead goes straight to `to`.
pub fn drag_path(from: Point, to: Point) -> Vec<Point> {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let distance = dx.hypot(dy);
    if distance <= DRAG_LEAD {
        return vec![to];
    }
    let along = |d: f64| Point { x: from.x + dx * d / distance, y: from.y + dy * d / distance };
    let mut path = vec![along(DRAG_LEAD)];
    path.extend(
        (1..DRAG_STEPS).map(|i| distance * i as f64 / DRAG_STEPS as f64).filter(|d| *d > DRAG_LEAD).map(along),
    );
    path.push(to);
    path
}

fn centre(out: Rect) -> Point {
    Point { x: (out.x + out.width / 2) as f64, y: (out.y + out.height / 2) as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect { x, y, width, height }
    }

    const HDMI: Rect = rect(0, 0, 1920, 1080);
    const EDP: Rect = rect(1920, 0, 1920, 1080);

    fn at(x: f64, y: f64) -> Point {
        Point { x, y }
    }

    /// Plans to `target`, then checks the simulated pointer ends within 1 px of it from every
    /// start in `starts` (the pointer may begin anywhere in the layout).
    fn assert_reaches(outputs: &[Rect], target: Point, starts: &[Point]) {
        let steps = plan(outputs, target).unwrap();
        for start in starts {
            let end = simulate(outputs, *start, &steps);
            assert!(
                (end.x - target.x).abs() <= 1.0 && (end.y - target.y).abs() <= 1.0,
                "from {start:?} ended at {end:?}, wanted {target:?}"
            );
        }
    }

    fn starts(outputs: &[Rect]) -> Vec<Point> {
        outputs
            .iter()
            .flat_map(|o| {
                [(0.0, 0.0), (0.5, 0.5), (0.99, 0.99), (0.2, 0.9)].map(|(fx, fy)| {
                    at(o.x as f64 + fx * (o.width - 1) as f64, o.y as f64 + fy * (o.height - 1) as f64)
                })
            })
            .collect()
    }

    #[test]
    fn a_drag_leads_past_the_threshold_then_steps_on_to_the_target() {
        for (from, to) in [(at(100.0, 100.0), at(300.0, 100.0)), (at(500.0, 500.0), at(2500.0, 700.0)), (at(10.0, 10.0), at(50.0, 40.0))] {
            let path = drag_path(from, to);
            let distance = |p: Point| (p.x - from.x).hypot(p.y - from.y);
            assert!((distance(path[0]) - DRAG_LEAD).abs() < 1e-9, "{from:?} -> {to:?}: lead {:?}", path[0]);
            // Motions keep coming after the lead, each farther along, ending on the target.
            assert!(path.len() >= 4, "{from:?} -> {to:?}: {path:?}");
            assert!(path.windows(2).all(|pair| distance(pair[1]) >= distance(pair[0]) + 2.0), "{path:?}");
            assert_eq!(*path.last().unwrap(), to);
        }
        // Shorter than the lead: straight to the target.
        assert_eq!(drag_path(at(0.0, 0.0), at(5.0, 0.0)), [at(5.0, 0.0)]);
    }

    #[test]
    fn two_side_by_side_outputs_cross_the_shared_edge_then_land_on_the_target() {
        let outputs = [HDMI, EDP];
        let steps = plan(&outputs, at(2070.0, 300.0)).unwrap();
        // 80 pushes, then: absolute at HDMI's right edge, nudge right, absolute inside eDP.
        assert_eq!(steps.len(), 80 + 3);
        assert_eq!(steps[0], Step::Relative { dx: -400, dy: 0 });
        assert_eq!(steps[20], Step::Relative { dx: 0, dy: -400 });
        assert_eq!(steps[80], Step::Absolute { x: 1.0, y: 540.0 / 1079.0 });
        assert_eq!(steps[81], Step::Relative { dx: 40, dy: 0 });
        let Step::Absolute { x, y } = steps[82] else { panic!("last step is absolute") };
        assert!((x - 150.0 / 1919.0).abs() < 1e-9 && (y - 300.0 / 1079.0).abs() < 1e-9);
        assert_reaches(&outputs, at(2070.0, 300.0), &starts(&outputs));
        assert_reaches(&outputs, at(10.0, 1070.0), &starts(&outputs));
    }

    #[test]
    fn a_single_output_needs_only_the_pushes_and_one_absolute_move() {
        let outputs = [HDMI];
        let steps = plan(&outputs, at(960.0, 540.0)).unwrap();
        assert_eq!(steps.len(), 81);
        assert!(matches!(steps[80], Step::Absolute { .. }));
        assert_reaches(&outputs, at(960.0, 540.0), &starts(&outputs));
        assert_reaches(&outputs, at(1919.0, 1079.0), &starts(&outputs));
    }

    #[test]
    fn stacked_outputs_cross_vertically() {
        let outputs = [HDMI, rect(0, 1080, 1920, 1080)];
        assert_reaches(&outputs, at(100.0, 1500.0), &starts(&outputs));
        assert_reaches(&outputs, at(100.0, 100.0), &starts(&outputs));
        let steps = plan(&outputs, at(100.0, 1500.0)).unwrap();
        assert!(steps.contains(&Step::Relative { dx: 0, dy: 40 }));
    }

    #[test]
    fn an_l_shaped_layout_routes_through_the_corner_output() {
        // C sits under A and B is right of A; going from C to B passes through A.
        let outputs = [rect(0, 0, 1920, 1080), rect(1920, 0, 1280, 1024), rect(0, 1080, 1280, 1024)];
        assert_reaches(&outputs, at(3000.0, 900.0), &starts(&outputs));
        assert_reaches(&outputs, at(1000.0, 2000.0), &starts(&outputs));
        assert_reaches(&outputs, at(5.0, 5.0), &starts(&outputs));
    }

    #[test]
    fn staggered_outputs_settle_on_the_same_corner_from_any_start() {
        let outputs = [rect(0, 0, 1920, 1080), rect(1920, 540, 1920, 1080)];
        assert_reaches(&outputs, at(3000.0, 1000.0), &starts(&outputs));
        assert_reaches(&outputs, at(1900.0, 20.0), &starts(&outputs));
    }

    #[test]
    fn a_target_outside_every_output_or_across_a_gap_is_an_error() {
        let outputs = [HDMI, EDP];
        assert!(plan(&outputs, at(-1.0, 10.0)).is_err());
        assert!(plan(&outputs, at(3840.0, 10.0)).is_err());
        assert!(plan(&outputs, at(10.0, 1080.0)).is_err());
        let gap = [HDMI, rect(2000, 0, 1920, 1080)];
        assert!(plan(&gap, at(100.0, 100.0)).is_ok());
        assert!(plan(&gap, at(2100.0, 100.0)).is_err());
        let corner_touch = [HDMI, rect(1920, 1080, 1920, 1080)];
        assert!(plan(&corner_touch, at(2000.0, 1200.0)).is_err());
        assert!(plan(&[], at(0.0, 0.0)).is_err());
    }

    #[test]
    fn simulate_clamps_relative_motion_and_maps_absolute_onto_the_output_under_the_pointer() {
        let outputs = [HDMI, EDP];
        let left = [Step::Relative { dx: -8000, dy: 0 }];
        assert_eq!(simulate(&outputs, at(2500.0, 300.0), &left), at(0.0, 300.0));
        let across = [Step::Relative { dx: 40, dy: 0 }];
        assert_eq!(simulate(&outputs, at(1919.0, 10.0), &across), at(1959.0, 10.0));
        let abs = [Step::Absolute { x: 0.0, y: 1.0 }];
        assert_eq!(simulate(&outputs, at(2500.0, 300.0), &abs), at(1920.0, 1079.0));
        assert_eq!(simulate(&outputs, at(100.0, 300.0), &abs), at(0.0, 1079.0));
    }

    #[test]
    fn plan_from_a_known_start_skips_the_pushes() {
        let outputs = [HDMI, EDP];
        let same = plan_from(&outputs, at(100.0, 100.0), at(900.0, 500.0)).unwrap();
        assert_eq!(same.len(), 1);
        assert!(matches!(same[0], Step::Absolute { .. }));
        let across = plan_from(&outputs, at(100.0, 100.0), at(2070.0, 300.0)).unwrap();
        assert_eq!(across.len(), 3);
        for (start, target) in [((100.0, 100.0), (2070.0, 300.0)), ((2500.0, 900.0), (5.0, 1070.0)), ((900.0, 5.0), (901.0, 6.0))] {
            let end = simulate(&outputs, at(start.0, start.1), &plan_from(&outputs, at(start.0, start.1), at(target.0, target.1)).unwrap());
            assert!((end.x - target.0).abs() <= 1.0 && (end.y - target.1).abs() <= 1.0, "{start:?} -> {end:?}");
        }
        assert!(plan_from(&outputs, at(-5.0, 0.0), at(10.0, 10.0)).is_err());
        assert!(plan_from(&outputs, at(10.0, 10.0), at(4000.0, 10.0)).is_err());
    }
}
