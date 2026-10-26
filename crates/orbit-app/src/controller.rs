//! Event handling: turns events into effects.
//!
//! The controller owns the [`AppState`] and never calls the platform. It
//! runs on the main thread and handles one event at a time; adapters send
//! [`AppEvent`]s and carry out the returned [`Effect`]s in order.
//!
//! Events of a gesture apply only while their [`GestureId`] is the held
//! press and the menu belongs to that press; answers of the target resolver
//! and the parameters form apply only while the menu waits for them under the
//! same id. Anything else is a leftover and is dropped without effects (I4).

use std::mem;
use std::time::Instant;

use orbit_domain::action::{ActionId, MenuItem};
use orbit_domain::geometry::{Hit, MenuGeometry, ScreenPoint, ScreenRect, place_menu};
use orbit_domain::operation::{FileName, OperationSpec, ResizeBound};
use orbit_domain::target::{FileTarget, PixelSize};
use tracing::{debug, warn};

use crate::catalog::{MenuModel, SLOT_COUNT, SlotModel, menu_slots};
use crate::messages::{
    AppEvent, Effect, GestureId, JobError, JobId, JobOutput, JobSpec, Notice, ParameterError,
    ParameterInput, ParametersKind, ParametersModel, Progress, ResolveError,
};
use crate::state::{AppState, Highlight, JobState, MenuState};

/// Turns [`AppEvent`]s into state changes and [`Effect`]s.
#[derive(Debug)]
pub struct AppController {
    state: AppState,
    geometry: MenuGeometry,
    next_job: JobId,
}

impl Default for AppController {
    fn default() -> Self {
        Self::new()
    }
}

impl AppController {
    /// A controller with nothing open and the standard menu geometry.
    pub fn new() -> Self {
        Self {
            state: AppState::default(),
            geometry: MenuGeometry::standard(),
            next_job: JobId::FIRST,
        }
    }

    /// Current state, for adapters that need to inspect it and for tests.
    pub const fn state(&self) -> &AppState {
        &self.state
    }

    /// Handles one event and returns what the adapters must do, in order.
    pub fn update(&mut self, event: AppEvent) -> Vec<Effect> {
        let mut effects = Vec::new();
        match event {
            AppEvent::Invoked {
                gesture,
                point,
                screens,
                pressed_at,
            } => self.invoked(gesture, point, &screens, pressed_at, &mut effects),
            AppEvent::PointerMoved { gesture, point } => {
                self.pointer_moved(gesture, point, &mut effects);
            }
            AppEvent::Released { gesture, point } => self.released(gesture, point, &mut effects),
            AppEvent::GestureCancelled { gesture } => {
                self.gesture_cancelled(gesture, &mut effects);
            }
            AppEvent::TargetResolved { gesture, result } => {
                self.target_resolved(gesture, result, &mut effects);
            }
            AppEvent::ParametersSubmitted { gesture, input } => {
                self.parameters_submitted(gesture, input, &mut effects);
            }
            AppEvent::ParametersCancelled { gesture } => {
                self.parameters_cancelled(gesture, &mut effects);
            }
            AppEvent::JobProgress { job, progress } => {
                self.job_progress(job, progress, &mut effects);
            }
            AppEvent::JobFinished { job, result } => self.job_finished(job, result, &mut effects),
            AppEvent::CancelJobRequested { job } => self.cancel_job(job, &mut effects),
        }
        effects
    }

    fn invoked(
        &mut self,
        gesture: GestureId,
        point: ScreenPoint,
        screens: &[ScreenRect],
        pressed_at: Instant,
        effects: &mut Vec<Effect>,
    ) {
        match self.state.gesture {
            Some(held) if held == gesture => {
                debug!(?gesture, "repeated press while held ignored");
                return;
            }
            Some(held) => {
                warn!(
                    ?held,
                    ?gesture,
                    "new press before the release of the previous one"
                );
            }
            None => {}
        }
        self.close_menu(effects);
        self.state.gesture = Some(gesture);
        let center =
            place_menu(point, screens, self.geometry.outer_radius()).unwrap_or_else(|| {
                warn!(
                    ?point,
                    "no display for the menu; centering it on the cursor"
                );
                point
            });
        self.state.menu = MenuState::Resolving {
            gesture,
            invoke: point,
            center,
            cursor: None,
        };
        effects.push(Effect::ResolveTarget {
            gesture,
            pressed_at,
        });
    }

    fn target_resolved(
        &mut self,
        gesture: GestureId,
        result: Result<FileTarget, ResolveError>,
        effects: &mut Vec<Effect>,
    ) {
        let MenuState::Resolving {
            gesture: waiting,
            invoke,
            center,
            cursor,
        } = self.state.menu
        else {
            debug!(?gesture, "target result without a pending request dropped");
            return;
        };
        if waiting != gesture {
            debug!(
                ?gesture,
                ?waiting,
                "target result of another gesture dropped"
            );
            return;
        }
        self.state.menu = MenuState::Closed;
        let target = match result {
            Ok(target) => target,
            Err(error) => {
                effects.push(Effect::Notify(Notice::Refused(error)));
                return;
            }
        };
        let Some(name) = target.path.file_name() else {
            effects.push(Effect::Notify(Notice::NoFileName));
            return;
        };
        let title = name.to_string_lossy().into_owned();
        let slots = menu_slots(&target.facts);
        let highlight = cursor.map_or(Highlight::None, |cursor| {
            highlight_at(&self.geometry, &slots, center, invoke, cursor)
        });
        effects.push(Effect::PresentMenu(MenuModel {
            gesture,
            center,
            geometry: self.geometry,
            title,
            slots,
        }));
        if highlight != Highlight::None {
            effects.push(Effect::Highlight(highlight));
        }
        self.state.menu = MenuState::Open {
            gesture,
            invoke,
            center,
            target,
            slots,
            highlight,
        };
    }

    fn pointer_moved(&mut self, gesture: GestureId, point: ScreenPoint, effects: &mut Vec<Effect>) {
        if self.state.gesture != Some(gesture) {
            debug!(?gesture, "pointer move outside the held gesture dropped");
            return;
        }
        match &mut self.state.menu {
            MenuState::Resolving {
                gesture: owner,
                cursor,
                ..
            } if *owner == gesture => *cursor = Some(point),
            MenuState::Open {
                gesture: owner,
                invoke,
                center,
                slots,
                highlight,
                ..
            } if *owner == gesture => {
                let next = highlight_at(&self.geometry, slots, *center, *invoke, point);
                if next != *highlight {
                    *highlight = next;
                    effects.push(Effect::Highlight(next));
                }
            }
            _ => {}
        }
    }

    fn released(&mut self, gesture: GestureId, point: ScreenPoint, effects: &mut Vec<Effect>) {
        if self.state.gesture != Some(gesture) {
            debug!(?gesture, "release outside the held gesture dropped");
            return;
        }
        self.state.gesture = None;
        if self.menu_gesture() != Some(gesture) {
            // Closed after a refusal or Esc: the release only ends the press.
            return;
        }
        match mem::take(&mut self.state.menu) {
            // The late answer of the resolver is dropped (I4).
            MenuState::Closed | MenuState::Resolving { .. } => {}
            MenuState::Open {
                invoke,
                center,
                target,
                slots,
                ..
            } => {
                effects.push(Effect::HideMenu);
                let hit = self.geometry.hit(center, invoke, point);
                self.choose(gesture, hit, &slots, target, effects);
            }
            parameters @ MenuState::Parameters { .. } => {
                // A form only opens after the release of its press.
                warn!(?gesture, "release while the form of the same press is open");
                self.state.menu = parameters;
            }
        }
    }

    /// Runs what the slot under the release point does. Neutral zone,
    /// outside, Cancel and unavailable actions do nothing.
    fn choose(
        &mut self,
        gesture: GestureId,
        hit: Hit,
        slots: &[SlotModel; SLOT_COUNT],
        target: FileTarget,
        effects: &mut Vec<Effect>,
    ) {
        let Hit::Slot(index) = hit else { return };
        let Some(slot) = slots.get(index) else { return };
        let MenuItem::Action(action) = slot.item else {
            return;
        };
        if !slot.availability.is_available() {
            return;
        }
        match action {
            ActionId::CopyPath => effects.push(Effect::WriteClipboard(target.path)),
            ActionId::Reveal => effects.push(Effect::RevealFile(target.path)),
            ActionId::ShowInfo => effects.push(Effect::ShowInfo(target)),
            ActionId::Rename => {
                let current = target
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.open_parameters(gesture, target, ParametersKind::Rename { current }, effects);
            }
            ActionId::ResizeImage => {
                // Available only for images, so the size is there.
                let Some(image) = target.facts.image else {
                    return;
                };
                let kind = ParametersKind::Resize { size: image.size };
                self.open_parameters(gesture, target, kind, effects);
            }
            ActionId::ArchiveZip | ActionId::ConvertToPng | ActionId::ConvertToJpeg => {
                if let Some(operation) = OperationSpec::for_job_action(action) {
                    self.start_job(target, operation, effects);
                }
            }
        }
    }

    fn open_parameters(
        &mut self,
        gesture: GestureId,
        target: FileTarget,
        kind: ParametersKind,
        effects: &mut Vec<Effect>,
    ) {
        effects.push(Effect::PresentParameters(ParametersModel {
            gesture,
            kind: kind.clone(),
        }));
        self.state.menu = MenuState::Parameters {
            gesture,
            target,
            kind,
        };
    }

    fn gesture_cancelled(&mut self, gesture: GestureId, effects: &mut Vec<Effect>) {
        if self.state.gesture != Some(gesture) || self.menu_gesture() != Some(gesture) {
            debug!(?gesture, "Esc outside the held gesture dropped");
            return;
        }
        // The press stays held: a repeated press must not reopen the menu.
        match self.state.menu {
            MenuState::Resolving { .. } => self.state.menu = MenuState::Closed,
            MenuState::Open { .. } => {
                self.state.menu = MenuState::Closed;
                effects.push(Effect::HideMenu);
            }
            MenuState::Closed | MenuState::Parameters { .. } => {}
        }
    }

    fn parameters_submitted(
        &mut self,
        gesture: GestureId,
        input: ParameterInput,
        effects: &mut Vec<Effect>,
    ) {
        let MenuState::Parameters {
            gesture: owner,
            target,
            kind,
        } = &self.state.menu
        else {
            debug!(?gesture, "parameters without an open form dropped");
            return;
        };
        if *owner != gesture {
            debug!(?gesture, ?owner, "parameters of another form dropped");
            return;
        }
        let decision = match (kind, input) {
            (ParametersKind::Rename { .. }, ParameterInput::Rename(text)) => {
                rename_operation(target, &text)
            }
            (ParametersKind::Resize { size }, ParameterInput::Resize(bound)) => {
                resize_operation(*size, bound).map(Some)
            }
            (kind, input) => {
                debug!(?kind, ?input, "input of another form dropped");
                return;
            }
        };
        let operation = match decision {
            Ok(operation) => operation,
            Err(error) => {
                effects.push(Effect::ParametersInvalid { gesture, error });
                return;
            }
        };
        let MenuState::Parameters { target, .. } = mem::take(&mut self.state.menu) else {
            return;
        };
        effects.push(Effect::HideParameters);
        if let Some(operation) = operation {
            self.start_job(target, operation, effects);
        }
    }

    fn parameters_cancelled(&mut self, gesture: GestureId, effects: &mut Vec<Effect>) {
        match self.state.menu {
            MenuState::Parameters { gesture: owner, .. } if owner == gesture => {
                self.state.menu = MenuState::Closed;
                effects.push(Effect::HideParameters);
            }
            _ => debug!(?gesture, "form cancel without its form dropped"),
        }
    }

    fn start_job(
        &mut self,
        target: FileTarget,
        operation: OperationSpec,
        effects: &mut Vec<Effect>,
    ) {
        let Some(name) = target.path.file_name() else {
            effects.push(Effect::Notify(Notice::NoFileName));
            return;
        };
        let destination = operation.destination(name);
        let id = self.next_job;
        self.next_job = id.next();
        let spec = JobSpec {
            id,
            target,
            operation,
            destination,
        };
        self.state.jobs.insert(
            id,
            JobState {
                spec: spec.clone(),
                progress: Progress::Indeterminate,
                cancelling: false,
            },
        );
        effects.push(Effect::StartJob(spec));
    }

    fn job_progress(&mut self, job: JobId, progress: Progress, effects: &mut Vec<Effect>) {
        let Some(state) = self.state.jobs.get_mut(&job) else {
            debug!(job = job.get(), "progress of an unknown job dropped");
            return;
        };
        state.progress = progress;
        effects.push(Effect::JobProgress { job, progress });
    }

    fn job_finished(
        &mut self,
        job: JobId,
        result: Result<JobOutput, JobError>,
        effects: &mut Vec<Effect>,
    ) {
        let Some(state) = self.state.jobs.remove(&job) else {
            debug!(job = job.get(), "result of an unknown job dropped");
            return;
        };
        let operation = state.spec.operation;
        // A success that races a cancellation is still reported: the result
        // exists.
        let notice = match result {
            Ok(output) => Notice::JobSucceeded {
                job,
                operation,
                output,
            },
            Err(error) => Notice::JobFailed {
                job,
                operation,
                error,
            },
        };
        effects.push(Effect::Notify(notice));
    }

    fn cancel_job(&mut self, job: JobId, effects: &mut Vec<Effect>) {
        match self.state.jobs.get_mut(&job) {
            Some(state) if !state.cancelling => {
                state.cancelling = true;
                effects.push(Effect::CancelJob(job));
            }
            Some(_) => {}
            None => debug!(job = job.get(), "cancel of an unknown job dropped"),
        }
    }

    /// The press the current menu belongs to.
    const fn menu_gesture(&self) -> Option<GestureId> {
        match self.state.menu {
            MenuState::Closed => None,
            MenuState::Resolving { gesture, .. }
            | MenuState::Open { gesture, .. }
            | MenuState::Parameters { gesture, .. } => Some(gesture),
        }
    }

    /// Closes whatever is shown, without running anything.
    fn close_menu(&mut self, effects: &mut Vec<Effect>) {
        match mem::take(&mut self.state.menu) {
            MenuState::Closed | MenuState::Resolving { .. } => {}
            MenuState::Open { .. } => effects.push(Effect::HideMenu),
            MenuState::Parameters { .. } => effects.push(Effect::HideParameters),
        }
    }
}

fn highlight_at(
    geometry: &MenuGeometry,
    slots: &[SlotModel; SLOT_COUNT],
    center: ScreenPoint,
    invoke: ScreenPoint,
    cursor: ScreenPoint,
) -> Highlight {
    match geometry.hit(center, invoke, cursor) {
        Hit::Slot(index) => match slots.get(index) {
            Some(slot) if slot.availability.is_available() => Highlight::Active(index),
            Some(_) => Highlight::Inactive(index),
            None => Highlight::None,
        },
        Hit::Neutral | Hit::Outside => Highlight::None,
    }
}

/// `Ok(None)` when the name does not change: the form closes without a job.
fn rename_operation(
    target: &FileTarget,
    text: &str,
) -> Result<Option<OperationSpec>, ParameterError> {
    let new_name = FileName::new(text)?;
    let unchanged = target
        .path
        .file_name()
        .is_some_and(|current| new_name.is_unchanged(current));
    Ok((!unchanged).then_some(OperationSpec::Rename { new_name }))
}

/// Resize only makes images smaller, so the fixed side must shrink; the
/// other side then shrinks too.
fn resize_operation(size: PixelSize, bound: ResizeBound) -> Result<OperationSpec, ParameterError> {
    let smaller = match bound {
        ResizeBound::Width(width) => width < size.width,
        ResizeBound::Height(height) => height < size.height,
    };
    if !smaller {
        return Err(ParameterError::NotSmaller);
    }
    let size = size.scaled_to(bound)?;
    Ok(OperationSpec::ResizeImage { size })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    use orbit_domain::action::{CANCEL_SLOT, MENU_LAYOUT};
    use orbit_domain::operation::{Destination, NameError, OutputFormat};
    use orbit_domain::target::{
        FileFacts, FileIdentity, FileKind, FileVersion, ImageFacts, ImageFormat,
    };

    use super::*;
    use crate::messages::Fraction;

    const SCREEN: ScreenRect = ScreenRect::new(0.0, 0.0, 1440.0, 875.0);
    const MIDDLE: ScreenPoint = ScreenPoint::new(700.0, 400.0);
    const A: GestureId = GestureId(1);
    const B: GestureId = GestureId(2);

    fn size(width: u32, height: u32) -> PixelSize {
        match PixelSize::new(width, height) {
            Some(size) => size,
            None => panic!("zero side"),
        }
    }

    fn side(value: u32) -> NonZeroU32 {
        match NonZeroU32::new(value) {
            Some(value) => value,
            None => panic!("zero side"),
        }
    }

    fn target(path: &str, kind: FileKind, image: Option<ImageFacts>) -> FileTarget {
        FileTarget {
            path: PathBuf::from(path),
            identity: FileIdentity {
                device: 1,
                inode: 42,
            },
            version: FileVersion {
                size: 1024,
                modified: SystemTime::UNIX_EPOCH + Duration::from_secs(1_000),
            },
            facts: FileFacts { kind, image },
        }
    }

    fn plain(path: &str) -> FileTarget {
        target(path, FileKind::File, None)
    }

    fn image(path: &str, format: ImageFormat, width: u32, height: u32) -> FileTarget {
        let image = ImageFacts {
            format,
            size: size(width, height),
        };
        target(path, FileKind::File, Some(image))
    }

    fn slot_of(action: ActionId) -> usize {
        match MENU_LAYOUT
            .iter()
            .position(|item| *item == MenuItem::Action(action))
        {
            Some(slot) => slot,
            None => panic!("{action:?} is not in the menu"),
        }
    }

    /// A point in the middle of the ring over `slot`.
    fn slot_point(center: ScreenPoint, slot: usize) -> ScreenPoint {
        let angle = MenuGeometry::standard().slot_center(slot);
        ScreenPoint::new(center.x + 70.0 * angle.sin(), center.y + 70.0 * angle.cos())
    }

    fn started(effects: &[Effect]) -> &JobSpec {
        match effects {
            [.., Effect::StartJob(spec)] => spec,
            other => panic!("no job started: {other:?}"),
        }
    }

    fn first_candidate(spec: &JobSpec) -> Option<OsString> {
        match &spec.destination {
            Destination::Candidates(candidates) => candidates.clone().next(),
            Destination::Exact(name) => panic!("exact destination {name}"),
        }
    }

    struct Harness {
        controller: AppController,
        pressed_at: Instant,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                controller: AppController::new(),
                pressed_at: Instant::now(),
            }
        }

        fn send(&mut self, event: AppEvent) -> Vec<Effect> {
            self.controller.update(event)
        }

        fn menu(&self) -> &MenuState {
            &self.controller.state().menu
        }

        fn held(&self) -> Option<GestureId> {
            self.controller.state().gesture
        }

        fn invoke(&mut self, gesture: GestureId, point: ScreenPoint) -> Vec<Effect> {
            self.send(AppEvent::Invoked {
                gesture,
                point,
                screens: vec![SCREEN],
                pressed_at: self.pressed_at,
            })
        }

        fn resolve_target(&self, gesture: GestureId) -> Effect {
            Effect::ResolveTarget {
                gesture,
                pressed_at: self.pressed_at,
            }
        }

        fn resolve(
            &mut self,
            gesture: GestureId,
            result: Result<FileTarget, ResolveError>,
        ) -> Vec<Effect> {
            self.send(AppEvent::TargetResolved { gesture, result })
        }

        /// Invokes at `point`, resolves `target` and returns the menu
        /// center.
        fn open(
            &mut self,
            gesture: GestureId,
            point: ScreenPoint,
            target: FileTarget,
        ) -> ScreenPoint {
            assert_eq!(
                self.invoke(gesture, point),
                vec![self.resolve_target(gesture)]
            );
            match self.resolve(gesture, Ok(target)).as_slice() {
                [Effect::PresentMenu(model)] => model.center,
                other => panic!("menu not presented: {other:?}"),
            }
        }

        fn move_to(&mut self, gesture: GestureId, point: ScreenPoint) -> Vec<Effect> {
            self.send(AppEvent::PointerMoved { gesture, point })
        }

        fn release(&mut self, gesture: GestureId, point: ScreenPoint) -> Vec<Effect> {
            self.send(AppEvent::Released { gesture, point })
        }

        fn cancel(&mut self, gesture: GestureId) -> Vec<Effect> {
            self.send(AppEvent::GestureCancelled { gesture })
        }

        /// Opens the menu in the middle of the screen and releases over
        /// `action`.
        fn choose(
            &mut self,
            gesture: GestureId,
            target: FileTarget,
            action: ActionId,
        ) -> Vec<Effect> {
            let center = self.open(gesture, MIDDLE, target);
            self.release(gesture, slot_point(center, slot_of(action)))
        }

        fn submit(&mut self, gesture: GestureId, input: ParameterInput) -> Vec<Effect> {
            self.send(AppEvent::ParametersSubmitted { gesture, input })
        }

        fn finish(&mut self, job: JobId, result: Result<JobOutput, JobError>) -> Vec<Effect> {
            self.send(AppEvent::JobFinished { job, result })
        }
    }

    #[test]
    fn copy_path_runs_on_release_over_its_slot() {
        let mut h = Harness::new();
        assert_eq!(h.invoke(A, MIDDLE), vec![h.resolve_target(A)]);
        assert!(matches!(h.menu(), MenuState::Resolving { gesture: A, .. }));

        let effects = h.resolve(A, Ok(plain("/Users/me/a b.txt")));
        let [Effect::PresentMenu(model)] = effects.as_slice() else {
            panic!("menu not presented: {effects:?}");
        };
        assert_eq!(model.gesture, A);
        assert_eq!(model.center, MIDDLE);
        assert_eq!(model.title, "a b.txt");
        assert_eq!(model.slots, menu_slots(&plain("/x").facts));

        let copy = slot_of(ActionId::CopyPath);
        let point = slot_point(MIDDLE, copy);
        assert_eq!(
            h.move_to(A, point),
            vec![Effect::Highlight(Highlight::Active(copy))]
        );
        assert_eq!(
            h.release(A, point),
            vec![
                Effect::HideMenu,
                Effect::WriteClipboard(PathBuf::from("/Users/me/a b.txt"))
            ]
        );
        assert_eq!(h.menu(), &MenuState::Closed);
        assert_eq!(h.held(), None);
    }

    #[test]
    fn reveal_and_show_info_run_instantly() {
        let mut h = Harness::new();
        assert_eq!(
            h.choose(A, plain("/tmp/a"), ActionId::Reveal),
            vec![
                Effect::HideMenu,
                Effect::RevealFile(PathBuf::from("/tmp/a"))
            ]
        );
        assert_eq!(
            h.choose(B, plain("/tmp/a"), ActionId::ShowInfo),
            vec![Effect::HideMenu, Effect::ShowInfo(plain("/tmp/a"))]
        );
    }

    #[test]
    fn release_without_movement_in_a_shifted_menu_does_nothing() {
        // Near the corner the menu is shifted, and the invocation point lies
        // over the ring.
        let invoke = ScreenPoint::new(40.0, 40.0);
        let mut h = Harness::new();
        let center = h.open(A, invoke, plain("/tmp/a"));
        assert_eq!(center, ScreenPoint::new(110.0, 110.0));
        let far = ScreenPoint::new(-1000.0, -1000.0);
        assert!(matches!(
            MenuGeometry::standard().hit(center, far, invoke),
            Hit::Slot(_)
        ));
        assert_eq!(h.move_to(A, invoke), vec![]);
        assert_eq!(h.release(A, invoke), vec![Effect::HideMenu]);

        // A move below the threshold changes nothing either.
        h.open(B, invoke, plain("/tmp/a"));
        let nudged = ScreenPoint::new(44.0, 43.0);
        assert_eq!(h.move_to(B, nudged), vec![]);
        assert_eq!(h.release(B, nudged), vec![Effect::HideMenu]);

        // Beyond the threshold, the ring decides.
        let c = GestureId(3);
        let center = h.open(c, invoke, plain("/tmp/a"));
        let copy = slot_point(center, slot_of(ActionId::CopyPath));
        assert_eq!(
            h.release(c, copy),
            vec![
                Effect::HideMenu,
                Effect::WriteClipboard(PathBuf::from("/tmp/a"))
            ]
        );
    }

    #[test]
    fn release_while_resolving_drops_the_late_target() {
        let mut h = Harness::new();
        h.invoke(A, MIDDLE);
        assert_eq!(h.release(A, MIDDLE), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);
        assert_eq!(h.resolve(A, Ok(plain("/tmp/a"))), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);
    }

    #[test]
    fn target_of_an_earlier_gesture_is_dropped() {
        let mut h = Harness::new();
        h.invoke(A, MIDDLE);
        h.release(A, MIDDLE);
        assert_eq!(h.invoke(B, MIDDLE), vec![h.resolve_target(B)]);
        assert_eq!(h.resolve(A, Ok(plain("/tmp/a"))), vec![]);
        assert!(matches!(h.menu(), MenuState::Resolving { gesture: B, .. }));
        assert!(matches!(
            h.resolve(B, Ok(plain("/tmp/b"))).as_slice(),
            [Effect::PresentMenu(_)]
        ));
    }

    #[test]
    fn esc_while_resolving_drops_the_late_target() {
        let mut h = Harness::new();
        h.invoke(A, MIDDLE);
        assert_eq!(h.cancel(A), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);
        assert_eq!(h.resolve(A, Ok(plain("/tmp/a"))), vec![]);
        assert_eq!(h.held(), Some(A));
    }

    #[test]
    fn repeated_press_while_held_never_reopens() {
        let mut h = Harness::new();
        // While resolving.
        h.invoke(A, MIDDLE);
        assert_eq!(h.invoke(A, MIDDLE), vec![]);
        // While open.
        h.resolve(A, Ok(plain("/tmp/a")));
        assert_eq!(h.invoke(A, MIDDLE), vec![]);
        assert!(matches!(h.menu(), MenuState::Open { gesture: A, .. }));
        // After Esc in the open menu.
        assert_eq!(h.cancel(A), vec![Effect::HideMenu]);
        assert_eq!(h.invoke(A, MIDDLE), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);
        assert_eq!(h.release(A, MIDDLE), vec![]);

        // After a refusal.
        h.invoke(B, MIDDLE);
        h.resolve(B, Err(ResolveError::NothingSelected));
        assert_eq!(h.invoke(B, MIDDLE), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);
        h.release(B, MIDDLE);

        // After Esc while resolving.
        let c = GestureId(3);
        h.invoke(c, MIDDLE);
        h.cancel(c);
        assert_eq!(h.invoke(c, MIDDLE), vec![]);
        assert_eq!(h.menu(), &MenuState::Closed);

        // Only a new press after the release starts again.
        h.release(c, MIDDLE);
        let d = GestureId(4);
        assert_eq!(h.invoke(d, MIDDLE), vec![h.resolve_target(d)]);
    }

    #[test]
    fn leftover_events_of_an_earlier_gesture_are_dropped() {
        let mut h = Harness::new();
        h.open(A, MIDDLE, plain("/tmp/a"));
        h.release(A, MIDDLE);

        h.open(B, MIDDLE, plain("/tmp/b"));
        let zip = slot_point(MIDDLE, slot_of(ActionId::ArchiveZip));
        h.move_to(B, zip);
        let before = h.menu().clone();

        let copy = slot_point(MIDDLE, slot_of(ActionId::CopyPath));
        assert_eq!(h.release(A, copy), vec![]);
        assert_eq!(h.move_to(A, copy), vec![]);
        assert_eq!(h.cancel(A), vec![]);
        assert_eq!(h.menu(), &before);
        assert_eq!(h.held(), Some(B));
        assert!(h.controller.state().jobs.is_empty());
    }

    #[test]
    fn press_after_a_lost_release_closes_the_menu_without_action() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, plain("/tmp/a"));
        h.move_to(A, slot_point(center, slot_of(ActionId::CopyPath)));
        assert_eq!(
            h.invoke(B, MIDDLE),
            vec![Effect::HideMenu, h.resolve_target(B)]
        );
        assert_eq!(h.held(), Some(B));
        assert_eq!(h.release(A, MIDDLE), vec![]);
    }

    #[test]
    fn esc_then_release_over_an_action_does_nothing() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, plain("/tmp/a"));
        let copy = slot_point(center, slot_of(ActionId::CopyPath));
        h.move_to(A, copy);
        assert_eq!(h.cancel(A), vec![Effect::HideMenu]);
        assert_eq!(h.release(A, copy), vec![]);
        assert_eq!(h.held(), None);
        assert_eq!(h.menu(), &MenuState::Closed);
    }

    #[test]
    fn cancel_slot_neutral_zone_and_outside_do_nothing() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, plain("/tmp/a"));
        let cancel = slot_point(center, CANCEL_SLOT);
        assert_eq!(
            h.move_to(A, cancel),
            vec![Effect::Highlight(Highlight::Active(CANCEL_SLOT))]
        );
        assert_eq!(h.release(A, cancel), vec![Effect::HideMenu]);

        h.open(B, MIDDLE, plain("/tmp/a"));
        let neutral = ScreenPoint::new(MIDDLE.x + 15.0, MIDDLE.y);
        assert_eq!(h.release(B, neutral), vec![Effect::HideMenu]);

        let c = GestureId(3);
        h.open(c, MIDDLE, plain("/tmp/a"));
        let outside = ScreenPoint::new(MIDDLE.x + 300.0, MIDDLE.y);
        assert_eq!(h.release(c, outside), vec![Effect::HideMenu]);
        assert!(h.controller.state().jobs.is_empty());
    }

    #[test]
    fn unavailable_action_is_dimmed_and_does_nothing() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, image("/tmp/a.heic", ImageFormat::Other, 10, 10));
        let resize = slot_of(ActionId::ResizeImage);
        let point = slot_point(center, resize);
        assert_eq!(
            h.move_to(A, point),
            vec![Effect::Highlight(Highlight::Inactive(resize))]
        );
        assert_eq!(h.release(A, point), vec![Effect::HideMenu]);
        assert_eq!(h.menu(), &MenuState::Closed);
    }

    #[test]
    fn highlight_changes_only_when_the_slot_changes() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, plain("/tmp/a"));
        let copy = slot_of(ActionId::CopyPath);
        assert_eq!(
            h.move_to(A, slot_point(center, copy)),
            vec![Effect::Highlight(Highlight::Active(copy))]
        );
        let nearby = ScreenPoint::new(slot_point(center, copy).x + 1.0, slot_point(center, copy).y);
        assert_eq!(h.move_to(A, nearby), vec![]);
        assert_eq!(
            h.move_to(A, MIDDLE),
            vec![Effect::Highlight(Highlight::None)]
        );
    }

    #[test]
    fn moves_while_resolving_highlight_when_the_menu_opens() {
        let mut h = Harness::new();
        h.invoke(A, MIDDLE);
        let copy = slot_of(ActionId::CopyPath);
        assert_eq!(h.move_to(A, slot_point(MIDDLE, copy)), vec![]);
        let effects = h.resolve(A, Ok(plain("/tmp/a")));
        assert!(matches!(
            effects.as_slice(),
            [Effect::PresentMenu(_), Effect::Highlight(Highlight::Active(slot))] if *slot == copy
        ));
    }

    #[test]
    fn refusals_close_the_gesture_with_a_reason() {
        let errors = [
            ResolveError::FinderNotFrontmost,
            ResolveError::OutsideFileArea,
            ResolveError::NothingSelected,
            ResolveError::MultipleSelected,
            ResolveError::SelectionUnstable,
            ResolveError::AccessibilityDenied,
            ResolveError::InputMonitoringDenied,
            ResolveError::Failed("AXError(-25204)".into()),
        ];
        for error in errors {
            let mut h = Harness::new();
            h.invoke(A, MIDDLE);
            assert_eq!(
                h.resolve(A, Err(error.clone())),
                vec![Effect::Notify(Notice::Refused(error))]
            );
            assert_eq!(h.menu(), &MenuState::Closed);
            assert_eq!(h.release(A, MIDDLE), vec![]);
        }

        let mut h = Harness::new();
        h.invoke(A, MIDDLE);
        assert_eq!(
            h.resolve(A, Ok(target("/", FileKind::Directory, None))),
            vec![Effect::Notify(Notice::NoFileName)]
        );
        assert_eq!(h.menu(), &MenuState::Closed);
    }

    #[test]
    fn new_press_closes_the_form_and_its_answers_are_dropped() {
        let mut h = Harness::new();
        assert_eq!(
            h.choose(A, plain("/tmp/report.txt"), ActionId::Rename),
            vec![
                Effect::HideMenu,
                Effect::PresentParameters(ParametersModel {
                    gesture: A,
                    kind: ParametersKind::Rename {
                        current: "report.txt".into()
                    },
                }),
            ]
        );
        assert_eq!(h.held(), None);
        assert_eq!(
            h.invoke(B, MIDDLE),
            vec![Effect::HideParameters, h.resolve_target(B)]
        );
        assert_eq!(
            h.submit(A, ParameterInput::Rename("new.txt".into())),
            vec![]
        );
        assert_eq!(h.send(AppEvent::ParametersCancelled { gesture: A }), vec![]);
        assert!(matches!(h.menu(), MenuState::Resolving { gesture: B, .. }));
        assert!(h.controller.state().jobs.is_empty());
    }

    #[test]
    fn gesture_events_do_not_touch_an_open_form() {
        let mut h = Harness::new();
        h.choose(A, plain("/tmp/report.txt"), ActionId::Rename);
        let form = h.menu().clone();
        assert_eq!(h.cancel(A), vec![]);
        assert_eq!(h.release(A, MIDDLE), vec![]);
        assert_eq!(h.move_to(A, MIDDLE), vec![]);
        assert_eq!(h.menu(), &form);
    }

    #[test]
    fn rename_validates_and_starts_an_exact_job() {
        let mut h = Harness::new();
        h.choose(A, plain("/tmp/report.txt"), ActionId::Rename);
        let invalid = [
            ("", NameError::Empty),
            ("   ", NameError::Empty),
            ("..", NameError::Reserved),
            ("a/b", NameError::ContainsSlash),
            ("a:b", NameError::ContainsColon),
            ("a\0b", NameError::ContainsNul),
        ];
        for (text, error) in invalid {
            assert_eq!(
                h.submit(A, ParameterInput::Rename(text.into())),
                vec![Effect::ParametersInvalid {
                    gesture: A,
                    error: ParameterError::Name(error)
                }]
            );
            assert!(matches!(h.menu(), MenuState::Parameters { gesture: A, .. }));
        }
        // Input of the other form is ignored.
        assert_eq!(
            h.submit(A, ParameterInput::Resize(ResizeBound::Width(side(10)))),
            vec![]
        );

        let effects = h.submit(A, ParameterInput::Rename("отчёт.txt".into()));
        assert_eq!(effects.first(), Some(&Effect::HideParameters));
        let spec = started(&effects);
        let Ok(new_name) = FileName::new("отчёт.txt") else {
            panic!("valid name");
        };
        assert_eq!(
            spec.operation,
            OperationSpec::Rename {
                new_name: new_name.clone()
            }
        );
        assert_eq!(spec.destination, Destination::Exact(new_name));
        assert_eq!(h.menu(), &MenuState::Closed);
    }

    #[test]
    fn rename_to_the_same_name_closes_without_a_job() {
        let mut h = Harness::new();
        h.choose(A, plain("/tmp/report.txt"), ActionId::Rename);
        assert_eq!(
            h.submit(A, ParameterInput::Rename("report.txt".into())),
            vec![Effect::HideParameters]
        );
        assert!(h.controller.state().jobs.is_empty());
    }

    #[test]
    fn esc_in_the_form_closes_it() {
        let mut h = Harness::new();
        h.choose(A, plain("/tmp/report.txt"), ActionId::Rename);
        assert_eq!(
            h.send(AppEvent::ParametersCancelled { gesture: A }),
            vec![Effect::HideParameters]
        );
        assert_eq!(h.menu(), &MenuState::Closed);
        assert_eq!(h.submit(A, ParameterInput::Rename("x".into())), vec![]);
    }

    #[test]
    fn resize_only_shrinks() {
        let photo = || image("/tmp/photo.png", ImageFormat::Png, 4000, 3000);
        let mut h = Harness::new();
        assert!(matches!(
            h.choose(A, photo(), ActionId::ResizeImage).as_slice(),
            [
                Effect::HideMenu,
                Effect::PresentParameters(ParametersModel {
                    kind: ParametersKind::Resize { .. },
                    ..
                })
            ]
        ));
        for bound in [
            ResizeBound::Width(side(4000)),
            ResizeBound::Width(side(5000)),
            ResizeBound::Height(side(3000)),
        ] {
            assert_eq!(
                h.submit(A, ParameterInput::Resize(bound)),
                vec![Effect::ParametersInvalid {
                    gesture: A,
                    error: ParameterError::NotSmaller
                }]
            );
        }
        let effects = h.submit(A, ParameterInput::Resize(ResizeBound::Width(side(1000))));
        let spec = started(&effects);
        assert_eq!(
            spec.operation,
            OperationSpec::ResizeImage {
                size: size(1000, 750)
            }
        );
        assert_eq!(first_candidate(spec), Some("photo 1000x750.png".into()));

        h.choose(B, photo(), ActionId::ResizeImage);
        let effects = h.submit(B, ParameterInput::Resize(ResizeBound::Height(side(1000))));
        assert_eq!(
            started(&effects).operation,
            OperationSpec::ResizeImage {
                size: size(1333, 1000)
            }
        );

        // Upscaling far beyond u32 is refused before any arithmetic.
        let c = GestureId(3);
        h.choose(
            c,
            image("/tmp/line.png", ImageFormat::Png, 1, 10_000),
            ActionId::ResizeImage,
        );
        assert_eq!(
            h.submit(
                c,
                ParameterInput::Resize(ResizeBound::Width(side(1_000_000)))
            ),
            vec![Effect::ParametersInvalid {
                gesture: c,
                error: ParameterError::NotSmaller
            }]
        );
    }

    #[test]
    fn job_actions_start_jobs_that_outlive_the_menu() {
        let mut h = Harness::new();
        let effects = h.choose(
            A,
            target("/tmp/Docs", FileKind::Directory, None),
            ActionId::ArchiveZip,
        );
        assert_eq!(effects.first(), Some(&Effect::HideMenu));
        let spec = started(&effects).clone();
        assert_eq!(spec.operation, OperationSpec::ArchiveZip);
        assert_eq!(first_candidate(&spec), Some("Docs.zip".into()));
        assert_eq!(h.menu(), &MenuState::Closed);
        assert!(h.controller.state().jobs.contains_key(&spec.id));

        let effects = h.choose(
            B,
            image("/tmp/photo.png", ImageFormat::Png, 10, 10),
            ActionId::ConvertToJpeg,
        );
        let convert = started(&effects).clone();
        assert_eq!(
            convert.operation,
            OperationSpec::ConvertImage {
                format: OutputFormat::Jpeg {
                    quality: orbit_domain::operation::JPEG_QUALITY
                }
            }
        );
        assert_eq!(first_candidate(&convert), Some("photo.jpg".into()));
        assert_ne!(convert.id, spec.id);
        assert_eq!(h.controller.state().jobs.len(), 2);

        let half = Progress::Fraction(match Fraction::new(0.5) {
            Some(half) => half,
            None => panic!("0.5 is a fraction"),
        });
        assert_eq!(
            h.send(AppEvent::JobProgress {
                job: spec.id,
                progress: half
            }),
            vec![Effect::JobProgress {
                job: spec.id,
                progress: half
            }]
        );
        let output = JobOutput {
            path: PathBuf::from("/tmp/Docs.zip"),
        };
        assert_eq!(
            h.finish(spec.id, Ok(output.clone())),
            vec![Effect::Notify(Notice::JobSucceeded {
                job: spec.id,
                operation: OperationSpec::ArchiveZip,
                output,
            })]
        );
        assert!(!h.controller.state().jobs.contains_key(&spec.id));
        assert!(h.controller.state().jobs.contains_key(&convert.id));
    }

    #[test]
    fn events_of_unknown_jobs_are_dropped() {
        let mut h = Harness::new();
        let job = JobId::FIRST;
        assert_eq!(
            h.send(AppEvent::JobProgress {
                job,
                progress: Progress::Indeterminate
            }),
            vec![]
        );
        assert_eq!(h.finish(job, Err(JobError::Cancelled)), vec![]);
        assert_eq!(h.send(AppEvent::CancelJobRequested { job }), vec![]);
    }

    #[test]
    fn cancel_is_sent_once_and_a_racing_success_is_reported() {
        let mut h = Harness::new();
        let effects = h.choose(A, plain("/tmp/a.txt"), ActionId::ArchiveZip);
        let job = started(&effects).id;
        assert_eq!(
            h.send(AppEvent::CancelJobRequested { job }),
            vec![Effect::CancelJob(job)]
        );
        assert_eq!(h.send(AppEvent::CancelJobRequested { job }), vec![]);
        let output = JobOutput {
            path: PathBuf::from("/tmp/a.txt.zip"),
        };
        assert_eq!(
            h.finish(job, Ok(output.clone())),
            vec![Effect::Notify(Notice::JobSucceeded {
                job,
                operation: OperationSpec::ArchiveZip,
                output
            })]
        );
        assert!(h.controller.state().jobs.is_empty());
    }

    #[test]
    fn a_full_queue_is_reported_as_a_failed_job() {
        let mut h = Harness::new();
        let effects = h.choose(A, plain("/tmp/a.txt"), ActionId::ArchiveZip);
        let job = started(&effects).id;
        assert_eq!(
            h.finish(job, Err(JobError::QueueFull)),
            vec![Effect::Notify(Notice::JobFailed {
                job,
                operation: OperationSpec::ArchiveZip,
                error: JobError::QueueFull
            })]
        );
    }

    #[test]
    fn one_gesture_runs_at_most_one_operation() {
        let mut h = Harness::new();
        let center = h.open(A, MIDDLE, plain("/tmp/a.txt"));
        let zip = slot_point(center, slot_of(ActionId::ArchiveZip));
        assert_eq!(h.release(A, zip).len(), 2);
        assert_eq!(h.release(A, zip), vec![]);
        assert_eq!(h.move_to(A, zip), vec![]);
        assert_eq!(h.controller.state().jobs.len(), 1);
    }

    #[test]
    fn empty_display_list_centers_the_menu_on_the_cursor() {
        let mut h = Harness::new();
        h.send(AppEvent::Invoked {
            gesture: A,
            point: MIDDLE,
            screens: Vec::new(),
            pressed_at: h.pressed_at,
        });
        assert!(matches!(
            h.menu(),
            MenuState::Resolving { center, .. } if *center == MIDDLE
        ));
    }
}
