use std::cell::RefCell;
use std::rc::Rc;

use super::*;

/// One element of the fake tree.
#[derive(Default)]
struct Data {
    strings: Vec<(&'static str, &'static str)>,
    parent: Option<usize>,
    children: Vec<usize>,
    arrays: Vec<(&'static str, Vec<usize>)>,
    url: Option<Url>,
    focus: Option<usize>,
    fails: Option<AxError>,
}

#[derive(Default)]
struct Tree(RefCell<Vec<Data>>);

#[derive(Clone)]
struct Node {
    tree: Rc<Tree>,
    id: usize,
}

impl Node {
    fn at(&self, id: usize) -> Self {
        Self {
            tree: Rc::clone(&self.tree),
            id,
        }
    }

    fn with<R>(&self, f: impl FnOnce(&Data) -> R) -> R {
        let nodes = self.tree.0.borrow();
        match nodes.get(self.id) {
            Some(data) => f(data),
            None => panic!("no node {}", self.id),
        }
    }

    fn check(&self) -> Result<(), AxError> {
        self.with(|data| data.fails.map_or(Ok(()), Err))
    }
}

impl AxNode for Node {
    fn string(&self, name: &str) -> Result<Option<String>, AxError> {
        self.check()?;
        Ok(self.with(|data| {
            data.strings
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }))
    }

    fn element(&self, name: &str) -> Result<Option<Self>, AxError> {
        self.check()?;
        let id = self.with(|data| match name {
            "AXParent" => data.parent,
            "AXFocusedUIElement" => data.focus,
            _ => None,
        });
        Ok(id.map(|id| self.at(id)))
    }

    fn elements(&self, name: &str) -> Result<Option<Vec<Self>>, AxError> {
        self.check()?;
        let ids = self.with(|data| {
            if name == "AXChildren" {
                return Some(data.children.clone());
            }
            data.arrays
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, ids)| ids.clone())
        });
        Ok(ids.map(|ids| ids.into_iter().map(|id| self.at(id)).collect()))
    }

    fn url(&self, name: &str) -> Result<Option<Url>, AxError> {
        self.check()?;
        Ok(if name == "AXURL" {
            self.with(|data| data.url.clone())
        } else {
            None
        })
    }
}

/// Builds a fake Finder: an application with one window.
struct Finder {
    app: Node,
    window: usize,
}

impl Finder {
    fn new() -> Self {
        let tree = Rc::new(Tree::default());
        let app = Node { tree, id: 0 };
        let mut finder = Self { app, window: 0 };
        finder.app.tree.0.borrow_mut().push(Data {
            strings: vec![("AXRole", "AXApplication")],
            ..Data::default()
        });
        finder.window = finder.add(0, &[("AXRole", "AXWindow")]);
        finder
    }

    fn add(&mut self, parent: usize, strings: &[(&'static str, &'static str)]) -> usize {
        let mut nodes = self.app.tree.0.borrow_mut();
        let id = nodes.len();
        nodes.push(Data {
            strings: strings.to_vec(),
            parent: Some(parent),
            ..Data::default()
        });
        if let Some(parent) = nodes.get_mut(parent) {
            parent.children.push(id);
        }
        id
    }

    fn edit(&mut self, id: usize, f: impl FnOnce(&mut Data)) {
        if let Some(data) = self.app.tree.0.borrow_mut().get_mut(id) {
            f(data);
        }
    }

    fn focus(&mut self, id: usize) {
        self.edit(0, |app| app.focus = Some(id));
    }

    fn select(&mut self, container: usize, name: &'static str, items: Vec<usize>) {
        self.edit(container, |data| data.arrays.push((name, items)));
    }

    /// An item with its own file URL.
    fn file(&mut self, parent: usize, path: &str) -> usize {
        let id = self.add(parent, &[("AXRole", "AXImage")]);
        self.edit(id, |data| data.url = Some(Url::File(path.into())));
        id
    }

    fn read(&self) -> Result<PathBuf, ReadError> {
        selected_path(&self.app)
    }

    /// A list view with one row per path; the URL sits on the text field of
    /// the name cell. Returns the outline and the rows.
    fn list_view(&mut self, paths: &[&str]) -> (usize, Vec<usize>) {
        let split = self.add(self.window, &[("AXRole", "AXSplitGroup")]);
        let scroll = self.add(split, &[("AXRole", "AXScrollArea")]);
        let outline = self.add(
            scroll,
            &[("AXRole", "AXOutline"), ("AXIdentifier", "ListView")],
        );
        let rows = paths
            .iter()
            .map(|path| {
                let row = self.add(outline, &[("AXRole", "AXRow")]);
                let cell = self.add(row, &[("AXRole", "AXCell")]);
                self.file(cell, path);
                row
            })
            .collect();
        (outline, rows)
    }
}

#[test]
fn list_view_selection_reads_the_url_below_the_row() {
    let mut f = Finder::new();
    let (outline, rows) = f.list_view(&["/tmp/a", "/tmp/b"]);
    f.focus(outline);
    f.select(outline, "AXSelectedRows", vec![rows[1]]);
    assert_eq!(f.read(), Ok(PathBuf::from("/tmp/b")));
}

#[test]
fn icon_view_selection_comes_from_the_list_or_its_sections() {
    let mut f = Finder::new();
    let scroll = f.add(f.window, &[("AXRole", "AXScrollArea")]);
    let list = f.add(
        scroll,
        &[
            ("AXRole", "AXList"),
            ("AXSubrole", "AXCollectionList"),
            ("AXIdentifier", "IconView"),
        ],
    );
    let item = f.file(list, "/tmp/icon");
    f.focus(list);
    f.select(list, "AXSelectedChildren", vec![item]);
    assert_eq!(f.read(), Ok(PathBuf::from("/tmp/icon")));

    // Grouped icons: the sections hold the selection.
    let mut f = Finder::new();
    let list = f.add(
        f.window,
        &[
            ("AXRole", "AXList"),
            ("AXSubrole", "AXCollectionList"),
            ("AXIdentifier", "GalleryView"),
        ],
    );
    let section = f.add(
        list,
        &[("AXRole", "AXList"), ("AXSubrole", "AXSectionList")],
    );
    let item = f.file(section, "/tmp/grouped");
    f.select(list, "AXSelectedChildren", vec![]);
    f.select(section, "AXSelectedChildren", vec![item]);
    // Focus on the section climbs to the collection list.
    f.focus(section);
    assert_eq!(f.read(), Ok(PathBuf::from("/tmp/grouped")));
}

/// A column list below `scroll_areas` scroll areas of a column browser.
fn column_view(scroll_areas: usize) -> (Finder, usize) {
    let mut f = Finder::new();
    let mut parent = f.add(
        f.window,
        &[("AXRole", "AXBrowser"), ("AXIdentifier", "ColumnView")],
    );
    for _ in 0..scroll_areas {
        parent = f.add(parent, &[("AXRole", "AXScrollArea")]);
    }
    let list = f.add(parent, &[("AXRole", "AXList")]);
    let item = f.file(list, "/tmp/column");
    f.focus(list);
    f.select(list, "AXSelectedChildren", vec![item]);
    (f, list)
}

#[test]
fn column_view_needs_one_to_four_scroll_areas() {
    for scroll_areas in 1..=MAX_COLUMN_SCROLL_AREAS {
        let (f, _) = column_view(scroll_areas);
        assert_eq!(f.read(), Ok(PathBuf::from("/tmp/column")), "{scroll_areas}");
    }
    for scroll_areas in [0, MAX_COLUMN_SCROLL_AREAS + 1] {
        let (f, _) = column_view(scroll_areas);
        assert!(
            matches!(f.read(), Err(ReadError::Outside(_))),
            "{scroll_areas}"
        );
    }
}

#[test]
fn desktop_selection_is_read() {
    let mut f = Finder::new();
    let scroll = f.add(
        f.window,
        &[("AXRole", "AXScrollArea"), ("AXDescription", "desktop")],
    );
    let group = f.add(
        scroll,
        &[("AXRole", "AXGroup"), ("AXDescription", "desktop")],
    );
    let item = f.file(group, "/Users/me/Desktop/a.txt");
    f.focus(group);
    f.select(group, "AXSelectedChildren", vec![item]);
    assert_eq!(f.read(), Ok(PathBuf::from("/Users/me/Desktop/a.txt")));

    // A desktop-like group elsewhere is not the desktop.
    let mut f = Finder::new();
    let group = f.add(
        f.window,
        &[("AXRole", "AXGroup"), ("AXDescription", "desktop")],
    );
    f.focus(group);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
}

#[test]
fn focus_outside_a_file_area_is_refused() {
    // Search field, even with a URL around.
    let mut f = Finder::new();
    let (outline, rows) = f.list_view(&["/tmp/a"]);
    f.select(outline, "AXSelectedRows", rows.clone());
    let search = f.add(
        f.window,
        &[("AXRole", "AXTextField"), ("AXSubrole", "AXSearchField")],
    );
    f.focus(search);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));

    // A name being edited in a row of the list.
    let field = f.add(rows[0], &[("AXRole", "AXTextField")]);
    f.focus(field);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));

    // The sidebar.
    let sidebar = f.add(
        f.window,
        &[("AXRole", "AXOutline"), ("AXDescription", "sidebar")],
    );
    let place = f.file(sidebar, "/Applications");
    f.select(sidebar, "AXSelectedRows", vec![place]);
    f.focus(sidebar);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));

    // An unknown table, the browser itself, the window, nothing focused.
    let table = f.add(f.window, &[("AXRole", "AXTable")]);
    f.focus(table);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
    let browser = f.add(
        f.window,
        &[("AXRole", "AXBrowser"), ("AXIdentifier", "ColumnView")],
    );
    f.focus(browser);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
    f.focus(f.window);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
    f.edit(0, |app| app.focus = None);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
}

#[test]
fn area_search_stops_after_twelve_levels() {
    let mut f = Finder::new();
    let (outline, rows) = f.list_view(&["/tmp/a"]);
    f.select(outline, "AXSelectedRows", rows);
    let mut parent = outline;
    for _ in 0..MAX_DEPTH - 1 {
        parent = f.add(parent, &[("AXRole", "AXGroup")]);
    }
    f.focus(parent);
    assert_eq!(f.read(), Ok(PathBuf::from("/tmp/a")));
    let deeper = f.add(parent, &[("AXRole", "AXGroup")]);
    f.focus(deeper);
    assert!(matches!(f.read(), Err(ReadError::Outside(_))));
}

#[test]
fn selection_must_be_exactly_one_file() {
    let mut f = Finder::new();
    let (outline, rows) = f.list_view(&["/tmp/a", "/tmp/b"]);
    f.focus(outline);
    // The area does not report a selection at all.
    assert!(matches!(f.read(), Err(ReadError::Failed(_))));
    f.select(outline, "AXSelectedRows", vec![]);
    assert_eq!(f.read(), Err(ReadError::Nothing));
    f.edit(outline, |data| data.arrays = vec![("AXSelectedRows", rows)]);
    assert_eq!(f.read(), Err(ReadError::Multiple(2)));
}

#[test]
fn selected_item_needs_one_file_url() {
    // No URL at all.
    let mut f = Finder::new();
    let (outline, _) = f.list_view(&[]);
    let row = f.add(outline, &[("AXRole", "AXRow")]);
    f.focus(outline);
    f.select(outline, "AXSelectedRows", vec![row]);
    assert!(matches!(f.read(), Err(ReadError::Failed(_))));

    // Two different URLs below the row.
    f.file(row, "/tmp/a");
    f.file(row, "/tmp/b");
    assert!(matches!(f.read(), Err(ReadError::Failed(_))));

    // The same URL twice counts once.
    let mut f = Finder::new();
    let (outline, _) = f.list_view(&[]);
    let row = f.add(outline, &[("AXRole", "AXRow")]);
    f.file(row, "/tmp/a");
    f.file(row, "/tmp/a");
    f.focus(outline);
    f.select(outline, "AXSelectedRows", vec![row]);
    assert_eq!(f.read(), Ok(PathBuf::from("/tmp/a")));

    // A URL that is not a file.
    let mut f = Finder::new();
    let (outline, _) = f.list_view(&[]);
    let row = f.add(outline, &[("AXRole", "AXRow")]);
    f.edit(row, |data| data.url = Some(Url::Other));
    f.focus(outline);
    f.select(outline, "AXSelectedRows", vec![row]);
    assert!(matches!(f.read(), Err(ReadError::Failed(_))));
}

#[test]
fn accessibility_errors_are_told_apart() {
    let mut f = Finder::new();
    let (outline, rows) = f.list_view(&["/tmp/a"]);
    f.focus(outline);
    f.select(outline, "AXSelectedRows", rows);
    f.edit(outline, |data| data.fails = Some(AxError::Disabled));
    assert_eq!(f.read(), Err(ReadError::Denied));
    f.edit(outline, |data| data.fails = Some(AxError::CannotComplete));
    assert!(matches!(f.read(), Err(ReadError::Failed(_))));
    f.edit(0, |data| data.fails = Some(AxError::Disabled));
    assert_eq!(f.read(), Err(ReadError::Denied));
}
