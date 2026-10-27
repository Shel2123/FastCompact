//! Finding the selected object in the focused file area of Finder.
//!
//! The target is exactly one selected item in the Finder view that has
//! keyboard focus (I2-default); the cursor is not used. The file area is
//! recognised by the role and identifiers of its container, never by the
//! mere presence of a file URL: the search field, the sidebar and anything
//! unknown are refused even when they expose `AXURL`.
//!
//! The logic runs over [`AxNode`], so it is tested with a fake tree.

use std::path::PathBuf;

/// Walk at most this many levels up from the focused element.
const MAX_DEPTH: usize = 12;
/// Scroll areas allowed between a column list and its browser.
const MAX_COLUMN_SCROLL_AREAS: usize = 4;
/// Search at most this many levels below a selected item for its URL.
const MAX_DESCENT: usize = 3;
/// Inspect at most this many descendants of a selected item.
const MAX_DESCENDANTS: usize = 40;

/// Why an attribute could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AxError {
    /// Accessibility access is not granted.
    Disabled,
    /// Finder did not answer in time.
    CannotComplete,
    /// Any other error, with its code.
    Other(i32),
}

/// The value of a URL attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Url {
    /// A `file:` URL.
    File(PathBuf),
    /// Any other URL.
    Other,
}

/// An accessibility element. Missing and unsupported attributes are
/// `Ok(None)`.
pub(crate) trait AxNode: Sized {
    /// A string attribute.
    fn string(&self, name: &str) -> Result<Option<String>, AxError>;
    /// An attribute that holds one element.
    fn element(&self, name: &str) -> Result<Option<Self>, AxError>;
    /// An attribute that holds an array of elements.
    fn elements(&self, name: &str) -> Result<Option<Vec<Self>>, AxError>;
    /// A URL attribute.
    fn url(&self, name: &str) -> Result<Option<Url>, AxError>;
}

/// Why the selection gives no target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReadError {
    /// Accessibility access is not granted.
    Denied,
    /// Focus is not in a known file area.
    Outside(String),
    /// Nothing is selected.
    Nothing,
    /// More than one object is selected.
    Multiple(usize),
    /// Finder answered in a way that names no single file.
    Failed(String),
}

impl From<AxError> for ReadError {
    fn from(err: AxError) -> Self {
        match err {
            AxError::Disabled => Self::Denied,
            AxError::CannotComplete => Self::Failed("Finder did not answer in time".into()),
            AxError::Other(code) => Self::Failed(format!("accessibility error {code}")),
        }
    }
}

/// The known Finder views that hold files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Area {
    /// List view.
    List,
    /// Icon or gallery view.
    Collection,
    /// One column of the column view.
    Column,
    /// The desktop.
    Desktop,
}

enum Step {
    Area(Area),
    Refuse(String),
    Up,
}

/// Attributes that identify a container.
struct Info {
    role: Option<String>,
    subrole: Option<String>,
    identifier: Option<String>,
    description: Option<String>,
}

impl Info {
    fn read(node: &impl AxNode) -> Result<Self, AxError> {
        Ok(Self {
            role: node.string("AXRole")?,
            subrole: node.string("AXSubrole")?,
            identifier: node.string("AXIdentifier")?,
            description: node.string("AXDescription")?,
        })
    }

    fn is(&self, role: &str) -> bool {
        self.role.as_deref() == Some(role)
    }
}

/// Reads the one selected object of the focused file area of `app`.
pub(crate) fn selected_path<N: AxNode>(app: &N) -> Result<PathBuf, ReadError> {
    let Some(focus) = app.element("AXFocusedUIElement")? else {
        return Err(ReadError::Outside(
            "Finder reports no focused element".into(),
        ));
    };
    let (area, container) = find_area(focus)?;
    let mut items = selected_items(area, &container)?;
    match items.len() {
        0 => Err(ReadError::Nothing),
        1 => items
            .pop()
            .map_or(Err(ReadError::Nothing), |item| item_path(&item)),
        count => Err(ReadError::Multiple(count)),
    }
}

/// Walks up from the focused element to the file area that holds it.
fn find_area<N: AxNode>(focus: N) -> Result<(Area, N), ReadError> {
    let mut current = Some(focus);
    for depth in 0..MAX_DEPTH {
        let Some(node) = current.take() else {
            break;
        };
        match classify(&node, depth)? {
            Step::Area(area) => return Ok((area, node)),
            Step::Refuse(reason) => {
                return Err(ReadError::Outside(format!("{reason} (depth {depth})")));
            }
            Step::Up => current = node.element("AXParent")?,
        }
    }
    Err(ReadError::Outside(
        "no Finder file area above the focused element".into(),
    ))
}

fn classify(node: &impl AxNode, depth: usize) -> Result<Step, AxError> {
    let info = Info::read(node)?;
    let subrole = info.subrole.as_deref();
    let identifier = info.identifier.as_deref();
    let description = info.description.as_deref();
    let step = match info.role.as_deref().unwrap_or_default() {
        "AXTextField" if subrole == Some("AXSearchField") => {
            Step::Refuse("focus in the search field".into())
        }
        // Focus never rests on a text field of a file area, except while a
        // name is edited in place.
        "AXTextField" | "AXTextArea" if depth == 0 => {
            Step::Refuse("focus in a text field, such as a name being edited".into())
        }
        "AXOutline" if identifier == Some("ListView") => Step::Area(Area::List),
        "AXOutline" if description == Some("sidebar") => {
            Step::Refuse("focus in the sidebar".into())
        }
        "AXList"
            if subrole == Some("AXCollectionList")
                && matches!(identifier, Some("IconView" | "GalleryView")) =>
        {
            Step::Area(Area::Collection)
        }
        // A section of an icon or gallery view; its collection list is above.
        "AXList" if subrole == Some("AXSectionList") => Step::Up,
        "AXList" if in_column_view(node)? => Step::Area(Area::Column),
        "AXBrowser" if identifier == Some("ColumnView") => {
            Step::Refuse("focus on the column browser itself; active column unknown".into())
        }
        "AXGroup" if description == Some("desktop") && is_desktop_scroll_area(node)? => {
            Step::Area(Area::Desktop)
        }
        role @ ("AXOutline" | "AXList" | "AXTable" | "AXBrowser") => Step::Refuse(format!(
            "focus in an unknown {role} (subrole {subrole:?}, identifier {identifier:?}, \
             description {description:?})"
        )),
        role @ ("AXWindow" | "AXSheet" | "AXApplication") => {
            Step::Refuse(format!("no file area between the focus and {role}"))
        }
        _ => Step::Up,
    };
    Ok(step)
}

/// A column list: `AXBrowser` (ColumnView) → one to
/// [`MAX_COLUMN_SCROLL_AREAS`] `AXScrollArea`s → `AXList`.
fn in_column_view(list: &impl AxNode) -> Result<bool, AxError> {
    let mut current = list.element("AXParent")?;
    let mut scroll_areas = 0;
    while let Some(node) = current {
        let info = Info::read(&node)?;
        if info.is("AXScrollArea") && scroll_areas < MAX_COLUMN_SCROLL_AREAS {
            scroll_areas += 1;
            current = node.element("AXParent")?;
            continue;
        }
        return Ok(scroll_areas > 0
            && info.is("AXBrowser")
            && info.identifier.as_deref() == Some("ColumnView"));
    }
    Ok(false)
}

fn is_desktop_scroll_area(group: &impl AxNode) -> Result<bool, AxError> {
    let Some(scroll) = group.element("AXParent")? else {
        return Ok(false);
    };
    let info = Info::read(&scroll)?;
    Ok(info.is("AXScrollArea") && info.description.as_deref() == Some("desktop"))
}

fn selected_items<N: AxNode>(area: Area, container: &N) -> Result<Vec<N>, ReadError> {
    let read = |node: &N, name: &str| match node.elements(name)? {
        Some(items) => Ok(items),
        None => Err(ReadError::Failed(format!("{area:?} area has no {name}"))),
    };
    match area {
        Area::List => read(container, "AXSelectedRows"),
        Area::Column | Area::Desktop => read(container, "AXSelectedChildren"),
        Area::Collection => {
            // The collection list may report the selection itself or leave it
            // to its section lists; both belong to the same view.
            if let Some(items) = container.elements("AXSelectedChildren")?
                && !items.is_empty()
            {
                return Ok(items);
            }
            let mut items = Vec::new();
            for section in container.elements("AXChildren")?.unwrap_or_default() {
                if section.string("AXSubrole")?.as_deref() == Some("AXSectionList") {
                    items.extend(read(&section, "AXSelectedChildren")?);
                }
            }
            Ok(items)
        }
    }
}

/// A selected item stands for one file: its own URL, or the one URL among
/// its descendants (list rows keep it on the text field of the name cell).
fn item_path(item: &impl AxNode) -> Result<PathBuf, ReadError> {
    let mut urls = Vec::new();
    match item.url("AXURL")? {
        Some(url) => urls.push(url),
        None => {
            let mut budget = MAX_DESCENDANTS;
            collect_urls(item, 1, &mut budget, &mut urls)?;
        }
    }
    match urls.as_slice() {
        [] => Err(ReadError::Failed("the selected item has no URL".into())),
        [Url::File(path)] => Ok(path.clone()),
        [Url::Other] => Err(ReadError::Failed("the selected item is not a file".into())),
        _ => Err(ReadError::Failed(format!(
            "the selected item has {} URLs",
            urls.len()
        ))),
    }
}

/// Depth-first search below `node`, bounded by [`MAX_DESCENT`] levels and
/// `budget` elements. Collects distinct URLs.
fn collect_urls(
    node: &impl AxNode,
    level: usize,
    budget: &mut usize,
    urls: &mut Vec<Url>,
) -> Result<(), AxError> {
    for child in node.elements("AXChildren")?.unwrap_or_default() {
        if *budget == 0 {
            return Ok(());
        }
        *budget -= 1;
        if let Some(url) = child.url("AXURL")?
            && !urls.contains(&url)
        {
            urls.push(url);
        }
        if level < MAX_DESCENT {
            collect_urls(&child, level + 1, budget, urls)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
