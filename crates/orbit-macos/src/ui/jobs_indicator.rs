//! Which jobs the menu bar shows, and how.
//!
//! A state machine without AppKit: the time comes in as a parameter, as in
//! the toast. A job is known from `job_started` to its result. It is shown
//! only once it has lasted [`SHOW_AFTER`], counted from `job_started` with
//! the time in the queue, so a short job is told only by its toast.

use std::time::{Duration, Instant};

use orbit_app::messages::{JobId, JobSpec, Progress};
use orbit_domain::operation::OperationSpec;

/// How long a job runs before the menu bar shows it.
pub const SHOW_AFTER: Duration = Duration::from_millis(500);

/// Longest file name in a row of the menu, in characters.
const MAX_NAME: usize = 40;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Queued, waiting for a worker.
    Queued,
    /// A worker runs it.
    Running(Progress),
}

/// A row of the menu for a shown job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    /// The job that the row cancels.
    pub job: JobId,
    /// What the row says.
    pub text: String,
}

/// What the item in the menu bar shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    /// No job is shown: the plain item.
    Idle,
    /// Shown jobs wait or run, with no amount done known.
    Busy,
    /// The shown jobs that know their amount done are this percent done,
    /// on average.
    Ring(u8),
}

#[derive(Debug)]
struct Job {
    id: JobId,
    name: String,
    operation: OperationSpec,
    started: Instant,
    phase: Phase,
    shown: bool,
}

impl Job {
    fn text(&self) -> String {
        let verb = verb(&self.operation);
        let name = &self.name;
        match self.phase {
            Phase::Queued => format!("{verb} “{name}” — waiting"),
            Phase::Running(Progress::Indeterminate) => format!("{verb} “{name}”…"),
            Phase::Running(Progress::Fraction(fraction)) => {
                format!("{verb} “{name}” — {}%", percent(fraction.get()))
            }
        }
    }
}

/// The jobs between `job_started` and their result, in the order they
/// started.
#[derive(Debug, Default)]
pub struct JobsIndicator {
    jobs: Vec<Job>,
}

impl JobsIndicator {
    /// No jobs.
    pub fn new() -> Self {
        Self::default()
    }

    /// A job was queued at `now`. A second start of the same id is ignored.
    pub fn started(&mut self, spec: &JobSpec, now: Instant) {
        if self.position(spec.id).is_some() {
            return;
        }
        self.jobs.push(Job {
            id: spec.id,
            name: short_name(&spec.target.path.file_name().map_or_else(
                || spec.target.path.to_string_lossy(),
                |name| name.to_string_lossy(),
            )),
            operation: spec.operation.clone(),
            started: now,
            phase: Phase::Queued,
            shown: false,
        });
    }

    /// A worker reported `progress`; the first report moves the job out of
    /// the queue. Unknown jobs are ignored.
    pub fn progress(&mut self, job: JobId, progress: Progress) {
        if let Some(index) = self.position(job) {
            self.jobs[index].phase = Phase::Running(progress);
        }
    }

    /// The job has its result and leaves. Returns whether it was known.
    pub fn finished(&mut self, job: JobId) -> bool {
        match self.position(job) {
            Some(index) => {
                self.jobs.remove(index);
                true
            }
            None => false,
        }
    }

    /// Shows the jobs that have lasted [`SHOW_AFTER`] by `now`.
    pub fn tick(&mut self, now: Instant) {
        for job in &mut self.jobs {
            if !job.shown && now >= job.started + SHOW_AFTER {
                job.shown = true;
            }
        }
    }

    /// The next moment a job passes [`SHOW_AFTER`], if any job is still
    /// hidden.
    pub fn deadline(&self) -> Option<Instant> {
        self.jobs
            .iter()
            .filter(|job| !job.shown)
            .map(|job| job.started + SHOW_AFTER)
            .min()
    }

    /// The rows of the shown jobs, in the order they started.
    pub fn rows(&self) -> Vec<JobRow> {
        self.jobs
            .iter()
            .filter(|job| job.shown)
            .map(|job| JobRow {
                job: job.id,
                text: job.text(),
            })
            .collect()
    }

    /// What the item in the menu bar shows.
    pub fn icon(&self) -> Icon {
        let mut shown = 0_usize;
        let mut known = 0_usize;
        let mut done = 0.0;
        for job in self.jobs.iter().filter(|job| job.shown) {
            shown += 1;
            if let Phase::Running(Progress::Fraction(fraction)) = job.phase {
                known += 1;
                done += fraction.get();
            }
        }
        match (shown, known) {
            (0, _) => Icon::Idle,
            (_, 0) => Icon::Busy,
            (_, known) => Icon::Ring(percent(done / count(known))),
        }
    }

    /// Every job, shown or not, in the order they started.
    pub fn jobs(&self) -> Vec<JobId> {
        self.jobs.iter().map(|job| job.id).collect()
    }

    /// Whether no job waits for its result.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    fn position(&self, job: JobId) -> Option<usize> {
        self.jobs.iter().position(|known| known.id == job)
    }
}

/// What a job does, as the start of its row.
fn verb(operation: &OperationSpec) -> &'static str {
    match operation {
        OperationSpec::ArchiveZip => "Compressing",
        OperationSpec::ConvertImage { .. } => "Converting",
        OperationSpec::ResizeImage { .. } => "Resizing",
        OperationSpec::Rename { .. } => "Renaming",
    }
}

/// Whole percent of `fraction`, rounded down, in `0..=100`.
fn percent(fraction: f64) -> u8 {
    let percent = (fraction.clamp(0.0, 1.0) * 100.0).floor();
    // In 0..=100 after the clamp, so the cast keeps the value.
    percent as u8
}

fn count(value: usize) -> f64 {
    // A handful of jobs: far below the integers an f64 keeps exactly.
    value as f64
}

/// `name` cut to [`MAX_NAME`] characters with an ellipsis.
fn short_name(name: &str) -> String {
    if name.chars().count() <= MAX_NAME {
        return name.to_owned();
    }
    let mut short: String = name.chars().take(MAX_NAME - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::SystemTime;

    use orbit_app::messages::Fraction;
    use orbit_domain::target::{FileFacts, FileIdentity, FileKind, FileTarget, FileVersion};

    use super::*;

    fn spec(id: u64, path: &str) -> JobSpec {
        let path = Path::new(path);
        let name = path.file_name().unwrap_or_default();
        JobSpec {
            id: JobId::new(id),
            target: FileTarget {
                path: path.to_owned(),
                identity: FileIdentity {
                    device: 1,
                    inode: id,
                },
                version: FileVersion {
                    size: 0,
                    modified: SystemTime::UNIX_EPOCH,
                },
                facts: FileFacts {
                    kind: FileKind::Directory,
                    image: None,
                },
            },
            operation: OperationSpec::ArchiveZip,
            destination: OperationSpec::ArchiveZip.destination(name),
        }
    }

    fn fraction(value: f64) -> Progress {
        Progress::Fraction(Fraction::new(value).unwrap_or_else(|| panic!("bad fraction {value}")))
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    fn texts(indicator: &JobsIndicator) -> Vec<String> {
        indicator.rows().into_iter().map(|row| row.text).collect()
    }

    #[test]
    fn a_job_is_shown_from_500_ms_after_its_start() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/Users/me/Docs"), start);
        assert_eq!(indicator.deadline(), Some(start + SHOW_AFTER));

        indicator.tick(start);
        assert!(indicator.rows().is_empty());
        indicator.tick(start + ms(499));
        assert!(indicator.rows().is_empty());
        assert_eq!(indicator.icon(), Icon::Idle);

        indicator.tick(start + ms(500));
        assert_eq!(texts(&indicator), ["Compressing “Docs” — waiting"]);
        assert_eq!(indicator.icon(), Icon::Busy);
        assert_eq!(indicator.deadline(), None);
    }

    #[test]
    fn a_queued_job_runs_and_reports_its_percent() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/Docs"), start);
        indicator.tick(start + SHOW_AFTER);
        indicator.progress(JobId::new(1), Progress::Indeterminate);
        assert_eq!(texts(&indicator), ["Compressing “Docs”…"]);
        assert_eq!(indicator.icon(), Icon::Busy);

        indicator.progress(JobId::new(1), fraction(0.429));
        assert_eq!(texts(&indicator), ["Compressing “Docs” — 42%"]);
        assert_eq!(indicator.icon(), Icon::Ring(42));
        indicator.progress(JobId::new(1), fraction(1.0));
        assert_eq!(indicator.icon(), Icon::Ring(100));
    }

    #[test]
    fn a_job_that_ends_before_the_threshold_is_never_shown() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/Docs"), start);
        indicator.progress(JobId::new(1), Progress::Indeterminate);
        indicator.tick(start + ms(300));
        assert!(indicator.finished(JobId::new(1)));
        indicator.tick(start + ms(900));
        assert!(indicator.rows().is_empty());
        assert_eq!(indicator.icon(), Icon::Idle);
        assert_eq!(indicator.deadline(), None);
        assert!(indicator.is_empty());
    }

    #[test]
    fn a_job_refused_by_a_full_queue_leaves_nothing() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        // The runtime reports QueueFull right after job_started.
        indicator.started(&spec(1, "/Docs"), start);
        assert!(indicator.finished(JobId::new(1)));
        assert!(indicator.is_empty());
        assert!(indicator.jobs().is_empty());
        assert_eq!(indicator.deadline(), None);
        assert!(!indicator.finished(JobId::new(1)), "finished twice");
    }

    #[test]
    fn a_quiet_job_is_shown_by_the_tick_alone() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/Docs"), start);
        indicator.progress(JobId::new(1), Progress::Indeterminate);
        // No more progress, as with ditto: only the timer shows it.
        indicator.tick(start + SHOW_AFTER);
        assert_eq!(texts(&indicator), ["Compressing “Docs”…"]);
    }

    #[test]
    fn several_jobs_keep_their_order_and_their_own_thresholds() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/a"), start);
        indicator.started(&spec(2, "/b"), start + ms(200));
        indicator.started(&spec(3, "/c"), start + ms(400));
        assert_eq!(indicator.deadline(), Some(start + ms(500)));
        indicator.tick(start + ms(700));
        assert_eq!(
            texts(&indicator),
            ["Compressing “a” — waiting", "Compressing “b” — waiting"]
        );
        assert_eq!(indicator.deadline(), Some(start + ms(900)));

        indicator.progress(JobId::new(1), fraction(0.2));
        indicator.progress(JobId::new(2), Progress::Indeterminate);
        // The job without an amount done does not lower the ring.
        assert_eq!(indicator.icon(), Icon::Ring(20));
        indicator.progress(JobId::new(2), fraction(0.6));
        assert_eq!(indicator.icon(), Icon::Ring(40));

        assert!(indicator.finished(JobId::new(1)));
        indicator.tick(start + ms(900));
        let rows: Vec<JobId> = indicator.rows().into_iter().map(|row| row.job).collect();
        assert_eq!(rows, [JobId::new(2), JobId::new(3)]);
        assert_eq!(indicator.jobs(), [JobId::new(2), JobId::new(3)]);
    }

    #[test]
    fn hidden_jobs_are_still_listed_for_quit() {
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, "/a"), start);
        indicator.started(&spec(2, "/b"), start);
        indicator.started(&spec(1, "/a"), start + ms(100));
        assert!(indicator.rows().is_empty());
        assert_eq!(indicator.jobs(), [JobId::new(1), JobId::new(2)]);
    }

    #[test]
    fn progress_of_an_unknown_job_is_ignored() {
        let mut indicator = JobsIndicator::new();
        indicator.progress(JobId::new(9), Progress::Indeterminate);
        assert!(indicator.is_empty());
    }

    #[test]
    fn long_names_are_cut() {
        let long = "x".repeat(60);
        let start = Instant::now();
        let mut indicator = JobsIndicator::new();
        indicator.started(&spec(1, &format!("/{long}")), start);
        indicator.tick(start + SHOW_AFTER);
        let expected = format!("Compressing “{}…” — waiting", "x".repeat(MAX_NAME - 1));
        assert_eq!(texts(&indicator), [expected]);
    }

    #[test]
    fn percent_rounds_down_and_stays_in_range() {
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.999), 99);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(1.5), 100);
    }
}
