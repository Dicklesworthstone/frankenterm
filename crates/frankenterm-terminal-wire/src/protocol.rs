use serde::{Deserialize, Serialize};

use crate::{
    BoundedVec, Id, MAX_CAPABILITIES, MAX_CHUNK_CELLS, MAX_CHUNK_LINES, MAX_CHUNKS, MAX_COLUMNS,
    MAX_FRAME_BYTES, MAX_PASTE_BYTES, MAX_RESOURCES, MAX_ROWS, MAX_SCREEN_CELLS,
    MAX_SNAPSHOT_BYTES, MAX_SUBSCRIPTIONS, PROTOCOL, Sha256, Text, WireError, WireI64, WireU64,
};

macro_rules! record {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty,)* }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    TerminalState,
    Input,
    Resize,
    Control,
    Events,
    /// References/metadata only; this does not advertise image decoding.
    ResourceReferences,
}

pub type Capabilities = BoundedVec<Capability, MAX_CAPABILITIES>;

pub fn validate_capabilities(capabilities: &Capabilities) -> Result<(), WireError> {
    for (index, item) in capabilities.as_slice().iter().enumerate() {
        if capabilities.as_slice()[..index].contains(item) {
            return Err(WireError::InvalidState);
        }
    }
    Ok(())
}

record!(PaneIdentity {
    host: Id,
    pane: Id,
    incarnation: Id
});
record!(Scope {
    identity: PaneIdentity,
    epoch: WireU64
});
record!(Hello {
    host: Id,
    hello_id: Id,
    asset: Sha256,
    required: Capabilities,
    optional: Capabilities,
});
record!(Welcome {
    host: Id,
    hello_id: Id,
    asset: Sha256,
    capabilities: Capabilities,
});

impl Hello {
    pub fn validate(&self) -> Result<(), WireError> {
        validate_capabilities(&self.required)?;
        validate_capabilities(&self.optional)?;
        if self
            .required
            .as_slice()
            .iter()
            .any(|item| self.optional.as_slice().contains(item))
        {
            return Err(WireError::InvalidState);
        }
        Ok(())
    }
}

record!(ClientFrame { protocol: Text<32>, session: Option<Id>, message: ClientMessage });
record!(ServerFrame { protocol: Text<32>, session: Option<Id>, message: ServerMessage });

impl ClientFrame {
    pub fn new(session: Option<Id>, message: ClientMessage) -> Result<Self, WireError> {
        Ok(Self {
            protocol: Text::new(PROTOCOL)?,
            session,
            message,
        })
    }

    pub fn validate(&self) -> Result<(), WireError> {
        if self.protocol.as_str() != PROTOCOL {
            return Err(WireError::UnsupportedVersion);
        }
        if matches!(&self.message, ClientMessage::Hello(_)) != self.session.is_none() {
            return Err(WireError::InvalidState);
        }
        self.message.validate()
    }
}

impl ServerFrame {
    pub fn new(session: Option<Id>, message: ServerMessage) -> Result<Self, WireError> {
        Ok(Self {
            protocol: Text::new(PROTOCOL)?,
            session,
            message,
        })
    }

    pub fn validate(&self) -> Result<(), WireError> {
        if self.protocol.as_str() != PROTOCOL {
            return Err(WireError::UnsupportedVersion);
        }
        if self.session.is_none() && !matches!(&self.message, ServerMessage::Error(_)) {
            return Err(WireError::InvalidState);
        }
        self.message.validate()
    }
}

record!(Subscribe { request_id: Id, panes: BoundedVec<PaneIdentity, MAX_SUBSCRIPTIONS> });
record!(Unsubscribe { request_id: Id, scopes: BoundedVec<Scope, MAX_SUBSCRIPTIONS> });
record!(ReadRequest {
    scope: Scope,
    request_id: Id
});
record!(AppliedAck {
    scope: Scope,
    sequence: WireU64
});
record!(Resync {
    scope: Scope,
    last_applied: WireU64,
    reason: ResyncReason
});
record!(Ping { nonce: WireU64 });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResyncReason {
    Gap,
    EpochChanged,
    BaselineMissing,
    SnapshotRejected,
    Trimmed,
}

// Mutation correlation is distinct from a read cursor. A valid wire fence still
// needs the actual mux-side lease and permission check before every effect.
record!(Mutation {
    scope: Scope,
    request_id: Id,
    lease_fence: WireU64
});
record!(LeaseRequest {
    scope: Scope,
    request_id: Id,
    lease_fence: WireU64
});
record!(Modifiers {
    shift: bool,
    alt: bool,
    control: bool,
    super_key: bool
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedKey {
    Enter,
    Tab,
    Escape,
    Backspace,
    Delete,
    Insert,
    Home,
    End,
    PageUp,
    PageDown,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Key {
    /// A bounded composed/IME commit, preserving combining and ZWJ sequences.
    Text(Text<4096>),
    Named(NamedKey),
    Function(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAction {
    Press,
    Release,
    Repeat,
}

record!(KeyRequest {
    effect: Mutation,
    key: Key,
    modifiers: Modifiers,
    action: KeyAction
});
record!(PasteRequest { effect: Mutation, text: Text<MAX_PASTE_BYTES> });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Press,
    Release,
    Move,
    Wheel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    None,
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

record!(MouseRequest {
    effect: Mutation,
    kind: MouseKind,
    button: MouseButton,
    column: u16,
    row: u16,
    wheel_rows: i16,
    modifiers: Modifiers,
});
record!(Dimensions {
    columns: u16,
    rows: u16,
    pixel_width: u32,
    pixel_height: u32
});

impl Dimensions {
    pub fn validate(&self) -> Result<(), WireError> {
        let cells = u32::from(self.columns)
            .checked_mul(u32::from(self.rows))
            .ok_or(WireError::InvalidBounds)?;
        if self.columns == 0
            || self.columns > MAX_COLUMNS
            || self.rows == 0
            || self.rows > MAX_ROWS
            || cells > MAX_SCREEN_CELLS
            || self.pixel_width > 65_536
            || self.pixel_height > 65_536
        {
            return Err(WireError::InvalidBounds);
        }
        Ok(())
    }

    pub fn cell_count(&self) -> u32 {
        u32::from(self.columns) * u32::from(self.rows)
    }
}

record!(ResizeRequest {
    effect: Mutation,
    dimensions: Dimensions
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ClientMessage {
    Hello(Hello),
    Subscribe(Subscribe),
    Unsubscribe(Unsubscribe),
    SnapshotRequest(ReadRequest),
    AppliedAck(AppliedAck),
    Resync(Resync),
    Key(KeyRequest),
    Paste(PasteRequest),
    Mouse(MouseRequest),
    Resize(ResizeRequest),
    AcquireControl(ReadRequest),
    RenewControl(LeaseRequest),
    ReleaseControl(LeaseRequest),
    Ping(Ping),
}

/// The consumer must still authorize this requirement against real grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredAuthority {
    None,
    View,
    Control,
    Resize,
}

impl ClientMessage {
    pub const fn required_authority(&self) -> RequiredAuthority {
        match self {
            Self::Hello(_) | Self::Ping(_) => RequiredAuthority::None,
            Self::Key(_)
            | Self::Paste(_)
            | Self::Mouse(_)
            | Self::AcquireControl(_)
            | Self::RenewControl(_)
            | Self::ReleaseControl(_) => RequiredAuthority::Control,
            Self::Resize(_) => RequiredAuthority::Resize,
            _ => RequiredAuthority::View,
        }
    }

    pub const fn capability(&self) -> Option<Capability> {
        match self {
            Self::Hello(_) | Self::Ping(_) => None,
            Self::Key(_) | Self::Paste(_) | Self::Mouse(_) => Some(Capability::Input),
            Self::Resize(_) => Some(Capability::Resize),
            Self::AcquireControl(_) | Self::RenewControl(_) | Self::ReleaseControl(_) => {
                Some(Capability::Control)
            }
            _ => Some(Capability::TerminalState),
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        match self {
            Self::SnapshotRequest(value) | Self::AcquireControl(value) => Some(&value.scope),
            Self::AppliedAck(value) => Some(&value.scope),
            Self::Resync(value) => Some(&value.scope),
            Self::Key(value) => Some(&value.effect.scope),
            Self::Paste(value) => Some(&value.effect.scope),
            Self::Mouse(value) => Some(&value.effect.scope),
            Self::Resize(value) => Some(&value.effect.scope),
            Self::RenewControl(value) | Self::ReleaseControl(value) => Some(&value.scope),
            _ => None,
        }
    }

    pub fn mutation(&self) -> Option<&Mutation> {
        match self {
            Self::Key(value) => Some(&value.effect),
            Self::Paste(value) => Some(&value.effect),
            Self::Mouse(value) => Some(&value.effect),
            Self::Resize(value) => Some(&value.effect),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::Hello(value) => value.validate()?,
            Self::Subscribe(value) => {
                if value.panes.is_empty() {
                    return Err(WireError::InvalidBounds);
                }
                for (index, pane) in value.panes.as_slice().iter().enumerate() {
                    if value.panes.as_slice()[..index].contains(pane) {
                        return Err(WireError::InvalidState);
                    }
                }
            }
            Self::Unsubscribe(value) => {
                if value.scopes.is_empty() {
                    return Err(WireError::InvalidBounds);
                }
                for (index, scope) in value.scopes.as_slice().iter().enumerate() {
                    if value.scopes.as_slice()[..index].contains(scope) {
                        return Err(WireError::InvalidState);
                    }
                }
            }
            Self::Key(value) => match &value.key {
                Key::Text(text)
                    if text.is_empty() || text.as_str().chars().any(char::is_control) =>
                {
                    return Err(WireError::InvalidBounds);
                }
                Key::Function(number) if !(1..=35).contains(number) => {
                    return Err(WireError::InvalidBounds);
                }
                _ => {}
            },
            Self::Paste(value) if value.text.is_empty() => {
                return Err(WireError::InvalidBounds);
            }
            Self::Mouse(value) => {
                if value.column >= MAX_COLUMNS
                    || value.row >= MAX_ROWS
                    || value.wheel_rows.unsigned_abs() > 120
                    || (value.kind == MouseKind::Wheel) != (value.wheel_rows != 0)
                {
                    return Err(WireError::InvalidBounds);
                }
            }
            Self::Resize(value) => value.dimensions.validate()?,
            Self::RenewControl(value) | Self::ReleaseControl(value) if value.lease_fence.0 == 0 => {
                return Err(WireError::StaleLease);
            }
            _ => {}
        }
        if self
            .mutation()
            .is_some_and(|value| value.lease_fence.0 == 0)
        {
            return Err(WireError::StaleLease);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb([u8; 3]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Underline {
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

pub const STYLE_BOLD: u16 = 1 << 0;
pub const STYLE_DIM: u16 = 1 << 1;
pub const STYLE_ITALIC: u16 = 1 << 2;
pub const STYLE_BLINK: u16 = 1 << 3;
pub const STYLE_REVERSE: u16 = 1 << 4;
pub const STYLE_INVISIBLE: u16 = 1 << 5;
pub const STYLE_STRIKETHROUGH: u16 = 1 << 6;
pub const STYLE_OVERLINE: u16 = 1 << 7;
pub const STYLE_MASK: u16 = STYLE_BOLD
    | STYLE_DIM
    | STYLE_ITALIC
    | STYLE_BLINK
    | STYLE_REVERSE
    | STYLE_INVISIBLE
    | STYLE_STRIKETHROUGH
    | STYLE_OVERLINE;

record!(CellStyle {
    foreground: Color,
    background: Color,
    underline_color: Color,
    underline: Underline,
    attributes: u16,
});

// Cell text preserves the complete UTF-8 grapheme. Width zero is an explicit
// wide-cell continuation immediately following a width-two lead cell.
record!(Cell {
    column: u16,
    text: Text<256>,
    width: u8,
    style: CellStyle,
    hyperlink: Option<WireU64>,
    image: Option<WireU64>,
});
record!(Line { row: u16, stable_row: WireI64, cells: BoundedVec<Cell, 512> });
record!(Hyperlink { id: WireU64, uri: Text<4096> });
record!(ImageReference {
    id: WireU64,
    digest: Sha256,
    pixel_width: u32,
    pixel_height: u32,
    media_type: Text<64>,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

record!(Cursor {
    column: u16,
    row: u16,
    visible: bool,
    blink: bool,
    shape: CursorShape
});
record!(TerminalModes {
    alternate_screen: bool,
    mouse_tracking: bool,
    bracketed_paste: bool
});
record!(GridPatch {
    dimensions: Dimensions,
    cursor: Cursor,
    lines: BoundedVec<Line, MAX_CHUNK_LINES>,
    hyperlinks: BoundedVec<Hyperlink, MAX_RESOURCES>,
    images: BoundedVec<ImageReference, MAX_RESOURCES>,
});

impl GridPatch {
    pub fn cell_count(&self) -> usize {
        self.lines
            .as_slice()
            .iter()
            .map(|line| line.cells.len())
            .sum()
    }

    pub fn validate(&self) -> Result<(), WireError> {
        self.dimensions.validate()?;
        validate_cursor(&self.cursor, &self.dimensions)?;
        if self.lines.is_empty() || self.cell_count() > MAX_CHUNK_CELLS {
            return Err(WireError::InvalidBounds);
        }
        for (index, link) in self.hyperlinks.as_slice().iter().enumerate() {
            if link.id.0 == 0
                || link.uri.is_empty()
                || self.hyperlinks.as_slice()[..index]
                    .iter()
                    .any(|old| old.id == link.id)
            {
                return Err(WireError::InvalidState);
            }
        }
        for (index, image) in self.images.as_slice().iter().enumerate() {
            if image.id.0 == 0
                || image.pixel_width == 0
                || image.pixel_height == 0
                || image.pixel_width > 65_536
                || image.pixel_height > 65_536
                || image.media_type.is_empty()
                || self.images.as_slice()[..index]
                    .iter()
                    .any(|old| old.id == image.id)
            {
                return Err(WireError::InvalidBounds);
            }
        }
        for (index, line) in self.lines.as_slice().iter().enumerate() {
            if line.row >= self.dimensions.rows
                || line.cells.len() != usize::from(self.dimensions.columns)
                || self.lines.as_slice()[..index]
                    .iter()
                    .any(|old| old.row == line.row || old.stable_row == line.stable_row)
            {
                return Err(WireError::InvalidBounds);
            }
            for (column, cell) in line.cells.as_slice().iter().enumerate() {
                if usize::from(cell.column) != column
                    || cell.width > 2
                    || cell.style.attributes & !STYLE_MASK != 0
                    || cell.text.as_str().chars().any(char::is_control)
                {
                    return Err(WireError::InvalidBounds);
                }
                if cell.width == 0 {
                    if !cell.text.is_empty() || column == 0 {
                        return Err(WireError::InvalidBounds);
                    }
                    let previous = &line.cells.as_slice()[column - 1];
                    if previous.width != 2
                        || previous.style != cell.style
                        || previous.hyperlink != cell.hyperlink
                        || previous.image != cell.image
                    {
                        return Err(WireError::InvalidBounds);
                    }
                } else if cell.text.is_empty()
                    || (cell.width == 2
                        && line
                            .cells
                            .as_slice()
                            .get(column + 1)
                            .is_none_or(|next| next.width != 0))
                {
                    return Err(WireError::InvalidBounds);
                }
                if cell.hyperlink.is_some_and(|id| {
                    !self
                        .hyperlinks
                        .as_slice()
                        .iter()
                        .any(|resource| resource.id == id)
                }) || cell.image.is_some_and(|id| {
                    !self
                        .images
                        .as_slice()
                        .iter()
                        .any(|resource| resource.id == id)
                }) {
                    return Err(WireError::InvalidState);
                }
            }
        }
        Ok(())
    }
}

fn validate_cursor(cursor: &Cursor, dimensions: &Dimensions) -> Result<(), WireError> {
    if cursor.column >= dimensions.columns || cursor.row >= dimensions.rows {
        return Err(WireError::InvalidBounds);
    }
    Ok(())
}

record!(Subscribed {
    scope: Scope,
    request_id: Id,
    sequence: WireU64,
    dimensions: Dimensions
});
record!(Unsubscribed {
    scope: Scope,
    request_id: Id
});
record!(SnapshotStart {
    scope: Scope,
    snapshot_id: WireU64,
    sequence: WireU64,
    dimensions: Dimensions,
    cursor: Cursor,
    modes: TerminalModes,
    title: Text<4096>,
    cwd: Option<Text<4096>>,
    chunks: u16,
    total_cells: u32,
    total_bytes: u32,
});
record!(SnapshotChunk {
    scope: Scope,
    snapshot_id: WireU64,
    sequence: WireU64,
    index: u16,
    chunks: u16,
    patch: GridPatch,
});
record!(SnapshotEnd {
    scope: Scope,
    snapshot_id: WireU64,
    sequence: WireU64,
    chunks: u16,
    total_cells: u32,
    total_bytes: u32,
});
record!(Delta {
    scope: Scope,
    base_sequence: WireU64,
    sequence: WireU64,
    modes: TerminalModes,
    patch: GridPatch,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptDisposition {
    Accepted,
    Denied,
    Indeterminate,
}

record!(InputReceipt {
    effect: Mutation,
    disposition: ReceiptDisposition,
    input_serial: Option<WireU64>,
    reason: Option<WireError>,
});
record!(ResizeReceipt {
    effect: Mutation,
    disposition: ReceiptDisposition,
    dimensions: Option<Dimensions>,
    reason: Option<WireError>,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseDisposition {
    Granted,
    Renewed,
    Released,
    Revoked,
    Denied,
}

record!(LeaseState {
    scope: Scope,
    request_id: Id,
    disposition: LeaseDisposition,
    fence: Option<WireU64>,
    ttl_ms: u32,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

record!(Detection {
    scope: Scope,
    event_id: WireU64,
    rule_id: Text<128>,
    severity: Severity,
    detected_at_ms: WireU64,
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneEventKind {
    Created,
    Closed,
    Resized,
    TitleChanged,
    Gap,
}

record!(PaneEvent {
    scope: Scope,
    event: PaneEventKind,
    title: Option<Text<4096>>,
    dimensions: Option<Dimensions>,
});
record!(ProtocolError {
    request_id: Option<Id>,
    scope: Option<Scope>,
    code: WireError,
});

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ServerMessage {
    Welcome(Welcome),
    Subscribed(Subscribed),
    Unsubscribed(Unsubscribed),
    SnapshotStart(SnapshotStart),
    SnapshotChunk(SnapshotChunk),
    SnapshotEnd(SnapshotEnd),
    Delta(Delta),
    ResyncRequired(Resync),
    InputReceipt(InputReceipt),
    ResizeReceipt(ResizeReceipt),
    LeaseState(LeaseState),
    Detection(Detection),
    PaneEvent(PaneEvent),
    Pong(Ping),
    Error(ProtocolError),
}

impl ServerMessage {
    pub const fn capability(&self) -> Option<Capability> {
        match self {
            Self::Welcome(_) | Self::Pong(_) | Self::Error(_) => None,
            Self::InputReceipt(_) => Some(Capability::Input),
            Self::ResizeReceipt(_) => Some(Capability::Resize),
            Self::LeaseState(_) => Some(Capability::Control),
            Self::Detection(_) | Self::PaneEvent(_) => Some(Capability::Events),
            _ => Some(Capability::TerminalState),
        }
    }

    pub fn scope(&self) -> Option<&Scope> {
        match self {
            Self::Subscribed(value) => Some(&value.scope),
            Self::Unsubscribed(value) => Some(&value.scope),
            Self::SnapshotStart(value) => Some(&value.scope),
            Self::SnapshotChunk(value) => Some(&value.scope),
            Self::SnapshotEnd(value) => Some(&value.scope),
            Self::Delta(value) => Some(&value.scope),
            Self::ResyncRequired(value) => Some(&value.scope),
            Self::InputReceipt(value) => Some(&value.effect.scope),
            Self::ResizeReceipt(value) => Some(&value.effect.scope),
            Self::LeaseState(value) => Some(&value.scope),
            Self::Detection(value) => Some(&value.scope),
            Self::PaneEvent(value) => Some(&value.scope),
            Self::Error(value) => value.scope.as_ref(),
            _ => None,
        }
    }

    pub fn patch(&self) -> Option<&GridPatch> {
        match self {
            Self::SnapshotChunk(value) => Some(&value.patch),
            Self::Delta(value) => Some(&value.patch),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), WireError> {
        match self {
            Self::Welcome(value) => validate_capabilities(&value.capabilities)?,
            Self::Subscribed(value) => value.dimensions.validate()?,
            Self::SnapshotStart(value) => {
                value.dimensions.validate()?;
                validate_cursor(&value.cursor, &value.dimensions)?;
                validate_snapshot_totals(
                    value.snapshot_id,
                    value.chunks,
                    value.total_cells,
                    value.total_bytes,
                )?;
                if value.total_cells != value.dimensions.cell_count()
                    || value.chunks > value.dimensions.rows
                    || usize::from(value.chunks) * MAX_CHUNK_LINES
                        < usize::from(value.dimensions.rows)
                {
                    return Err(WireError::InvalidBounds);
                }
            }
            Self::SnapshotChunk(value) => {
                if value.snapshot_id.0 == 0
                    || value.chunks == 0
                    || value.chunks > MAX_CHUNKS
                    || value.index >= value.chunks
                    || value.chunks > value.patch.dimensions.rows
                {
                    return Err(WireError::InvalidBounds);
                }
                value.patch.validate()?;
            }
            Self::SnapshotEnd(value) => {
                validate_snapshot_totals(
                    value.snapshot_id,
                    value.chunks,
                    value.total_cells,
                    value.total_bytes,
                )?;
            }
            Self::Delta(value) => {
                if value.sequence != value.base_sequence.next()? {
                    return Err(WireError::StaleSequence);
                }
                value.patch.validate()?;
            }
            Self::InputReceipt(value) => {
                if value.effect.lease_fence.0 == 0
                    || (value.disposition == ReceiptDisposition::Accepted)
                        != value.input_serial.is_some()
                    || value.input_serial.is_some_and(|serial| serial.0 == 0)
                    || (value.disposition == ReceiptDisposition::Accepted) == value.reason.is_some()
                {
                    return Err(WireError::InvalidState);
                }
            }
            Self::ResizeReceipt(value) => {
                if value.effect.lease_fence.0 == 0
                    || (value.disposition == ReceiptDisposition::Accepted)
                        != value.dimensions.is_some()
                    || (value.disposition == ReceiptDisposition::Accepted) == value.reason.is_some()
                {
                    return Err(WireError::InvalidState);
                }
                if let Some(dimensions) = &value.dimensions {
                    dimensions.validate()?;
                }
            }
            Self::LeaseState(value) => {
                let active = matches!(
                    value.disposition,
                    LeaseDisposition::Granted | LeaseDisposition::Renewed
                );
                let identified = value.disposition != LeaseDisposition::Denied;
                if identified != value.fence.is_some()
                    || value.fence.is_some_and(|fence| fence.0 == 0)
                    || (active && (value.ttl_ms == 0 || value.ttl_ms > 300_000))
                    || (!active && value.ttl_ms != 0)
                {
                    return Err(WireError::InvalidState);
                }
            }
            Self::Detection(value) if value.rule_id.is_empty() => {
                return Err(WireError::InvalidBounds);
            }
            Self::PaneEvent(value) => {
                if let Some(dimensions) = &value.dimensions {
                    dimensions.validate()?;
                }
                if (value.event == PaneEventKind::Resized) != value.dimensions.is_some()
                    || (value.event == PaneEventKind::TitleChanged) != value.title.is_some()
                {
                    return Err(WireError::InvalidState);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn validate_snapshot_totals(
    snapshot_id: WireU64,
    chunks: u16,
    cells: u32,
    bytes: u32,
) -> Result<(), WireError> {
    let chunks = usize::from(chunks);
    let cell_capacity = chunks
        .checked_mul(MAX_CHUNK_CELLS)
        .ok_or(WireError::InvalidBounds)?;
    let byte_capacity = chunks
        .checked_mul(MAX_FRAME_BYTES)
        .ok_or(WireError::InvalidBounds)?;
    if snapshot_id.0 == 0
        || chunks == 0
        || chunks > usize::from(MAX_CHUNKS)
        || cells == 0
        || cells > MAX_SCREEN_CELLS
        || bytes == 0
        || bytes > MAX_SNAPSHOT_BYTES
        || usize::try_from(cells).map_err(|_| WireError::InvalidBounds)? > cell_capacity
        || usize::try_from(bytes).map_err(|_| WireError::InvalidBounds)? > byte_capacity
        || usize::try_from(cells).map_err(|_| WireError::InvalidBounds)? < chunks
    {
        return Err(WireError::InvalidBounds);
    }
    Ok(())
}
