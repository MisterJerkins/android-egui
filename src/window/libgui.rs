use std::alloc::{Layout, alloc_zeroed};
use std::ffi::{CStr, c_char, c_void};
use std::mem::MaybeUninit;
use std::ptr::null_mut;
use std::sync::OnceLock;

use super::{ANativeWindow, Capture, FORMAT_RGBA_8888};
use crate::utils::{self, Library};
use crate::{Result, bail, err};

pub type Ptr = *mut c_void;

const FLAG_SKIP_SCREENSHOT: u32 = 0x40;
const FLAG_SECURE: u32 = 0x80;
const FLAG_CONTAINER: u32 = 0x4000;
const WINDOW_TYPE_DONT_SCREENSHOT: i32 = 441_731;
const METADATA_WINDOW_TYPE: u32 = 2;
const TRUSTED_OVERLAY_ENABLED: i32 = 2;
const OBJECT_SIZE: usize = 4096;
const GUARD: [u8; 32] = [0xA5; 32];

/// Non-trivial C++ values come back through a hidden pointer. A struct this large is
/// returned the same way on every Android ABI, so the callee sees the pointer it expects.
#[repr(C)]
struct Returned<T> {
    value: T,
    _pad: [MaybeUninit<usize>; 3],
}

#[repr(C, align(16))]
struct Storage<const N: usize>([u8; N]);

impl<const N: usize> Storage<N> {
    fn new() -> Self {
        Self([0; N])
    }

    fn ptr(&mut self) -> Ptr {
        self.0.as_mut_ptr().cast()
    }
}

type Retain = unsafe extern "C" fn(*const c_void, *const c_void);
type Method = unsafe extern "C" fn(Ptr);
type Status = unsafe extern "C" fn(Ptr) -> i32;
type GetShared = unsafe extern "C" fn() -> Returned<Ptr>;
type Shared = unsafe extern "C" fn(Ptr) -> Returned<Ptr>;
type TxControl = unsafe extern "C" fn(Ptr, *const Ptr) -> Ptr;
type TxInt = unsafe extern "C" fn(Ptr, *const Ptr, i32) -> Ptr;
type TxSize = unsafe extern "C" fn(Ptr, *const Ptr, u32, u32) -> Ptr;
type TxBool = unsafe extern "C" fn(Ptr, *const Ptr, bool) -> Ptr;

/// A strong reference to one of the system's reference-counted objects, given up when dropped.
pub struct Handle(Ptr);

unsafe impl Send for Handle {}

impl Handle {
    /// Takes over a reference the caller already holds.
    fn adopt(object: Ptr) -> Self {
        Self(object)
    }

    /// Takes a reference of its own.
    pub fn share(gui: &LibGui, object: Ptr) -> Self {
        gui.retain(object);
        Self(object)
    }

    pub fn as_ptr(&self) -> Ptr {
        self.0
    }
}

impl Clone for Handle {
    fn clone(&self) -> Self {
        match LibGui::get() {
            Ok(gui) => Self::share(gui, self.0),
            Err(_) => Self(null_mut()),
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if let (Ok(gui), false) = (LibGui::get(), self.0.is_null()) {
            gui.release(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Parent {
    Control,
    Handle,
}

#[derive(Clone, Copy, Debug)]
enum Create {
    Plain,
    WindowType(i32),
    Metadata,
    MetadataHint(Parent),
    CheckedWindowType,
    CheckedMetadata,
    CheckedMetadataHint(Parent),
}

const CREATE: &[(&CStr, Create)] = &[
    (
        c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEiRKNS4_INS_7IBinderEEERKNS_3gui13LayerMetadataEPj",
        Create::CheckedMetadataHint(Parent::Handle),
    ),
    (
        c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEiRKNS4_INS_7IBinderEEENS_3gui13LayerMetadataEPj",
        Create::CheckedMetadataHint(Parent::Handle),
    ),
    (
        c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEjRKNS4_INS_7IBinderEEENS_13LayerMetadataEPj",
        Create::CheckedMetadataHint(Parent::Handle),
    ),
    (
        c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEjPS5_NS_13LayerMetadataEPj",
        Create::CheckedMetadataHint(Parent::Control),
    ),
    (c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEjPS5_NS_13LayerMetadataE", Create::CheckedMetadata),
    (c"_ZN7android21SurfaceComposerClient20createSurfaceCheckedERKNS_7String8EjjiPNS_2spINS_14SurfaceControlEEEjPS5_ii", Create::CheckedWindowType),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjiiRKNS_2spINS_7IBinderEEERKNS_3gui13LayerMetadataEPj", Create::MetadataHint(Parent::Handle)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjiiRKNS_2spINS_7IBinderEEENS_3gui13LayerMetadataEPj", Create::MetadataHint(Parent::Handle)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjijRKNS_2spINS_7IBinderEEENS_13LayerMetadataEPj", Create::MetadataHint(Parent::Handle)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjijPNS_14SurfaceControlENS_13LayerMetadataEPj", Create::MetadataHint(Parent::Control)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjijPNS_14SurfaceControlENS_13LayerMetadataE", Create::Metadata),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjijPNS_14SurfaceControlEii", Create::WindowType(-1)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8EjjijPNS_14SurfaceControlEjj", Create::WindowType(0)),
    (c"_ZN7android21SurfaceComposerClient13createSurfaceERKNS_7String8Ejjij", Create::Plain),
];

type DisplayIds = unsafe extern "C" fn() -> Returned<[Ptr; 3]>;

enum Token {
    Internal { token: GetShared, ids: Option<DisplayIds> },
    Physical { ids: DisplayIds, token: unsafe extern "C" fn(u64) -> Returned<Ptr> },
    BuiltIn(unsafe extern "C" fn(i32) -> Returned<Ptr>),
}

/// The first id of the `std::vector<PhysicalDisplayId>` the composer returns: the main display.
unsafe fn main_display(ids: DisplayIds) -> Option<u64> {
    unsafe {
        let [begin, end, _] = ids().value;
        let id = (!begin.is_null() && begin != end).then(|| begin.cast::<u64>().read_unaligned());
        libc::free(begin);
        id
    }
}

type Query = unsafe extern "C" fn(*const Ptr, *mut u8) -> i32;

enum State {
    Modern(Query),
    Legacy(Query),
}

enum NewTransaction {
    Ndk(unsafe extern "C" fn() -> Ptr),
    Default(Method),
    Copy(unsafe extern "C" fn(Ptr, Ptr)),
}

enum Apply {
    Sync(unsafe extern "C" fn(Ptr, bool) -> i32),
    SyncOneWay(unsafe extern "C" fn(Ptr, bool, bool) -> i32),
}

enum Trusted {
    Bool(TxBool),
    Enum(TxInt),
}

enum CopyLayer {
    Plain(unsafe extern "C" fn(Ptr, Ptr) -> Returned<Ptr>),
    Cropped(unsafe extern "C" fn(Ptr, Ptr, Ptr, Ptr) -> Returned<Ptr>),
}

/// What it takes to show a copy of a layer on another display.
struct Mirroring {
    copy: CopyLayer,
    set_stack: unsafe extern "C" fn(Ptr, *const Ptr, u32) -> Ptr,
    reparent: unsafe extern "C" fn(Ptr, *const Ptr, *const Ptr) -> Ptr,
    set_matrix: unsafe extern "C" fn(Ptr, *const Ptr, f32, f32, f32, f32) -> Ptr,
    set_position: unsafe extern "C" fn(Ptr, *const Ptr, f32, f32) -> Ptr,
}

struct Transact {
    new: NewTransaction,
    set_layer: TxInt,
    show: TxControl,
    hide: TxControl,
    set_size: Option<TxSize>,
    trusted: Option<Trusted>,
    apply: Apply,
}

struct Global {
    open: unsafe extern "C" fn(),
    close: unsafe extern "C" fn(bool),
    set_layer: unsafe extern "C" fn(Ptr, i32) -> i32,
    show: Status,
    hide: Status,
    set_size: Option<unsafe extern "C" fn(Ptr, u32, u32) -> i32>,
}

enum Ops {
    Transact(Transact),
    Global(Global),
}

pub struct LibGui {
    api: u32,
    picks: Vec<(&'static str, &'static CStr)>,
    retain: Retain,
    release: Retain,
    string8_new: unsafe extern "C" fn(Ptr, *const c_char),
    string8_drop: Method,
    client_default: Option<GetShared>,
    client_new: Option<Method>,
    client_check: Option<Status>,
    create: (Ptr, Create),
    metadata_new: Option<Method>,
    metadata_set: Option<unsafe extern "C" fn(Ptr, u32, i32)>,
    validate: Option<Status>,
    surface: Shared,
    disconnect: Option<Method>,
    update_size: Option<unsafe extern "C" fn(Ptr, u32, u32)>,
    token: Token,
    state: State,
    ops: Ops,
    mirroring: Option<Mirroring>,
}

unsafe impl Send for LibGui {}
unsafe impl Sync for LibGui {}

struct Resolver<'a> {
    library: &'a Library,
    picks: Vec<(&'static str, &'static CStr)>,
}

impl Resolver<'_> {
    unsafe fn optional<F: Copy>(&mut self, what: &'static str, symbols: &[&'static CStr]) -> Option<F> {
        symbols.iter().find_map(|&symbol| {
            let function = unsafe { self.library.func(symbol) }?;
            self.picks.push((what, symbol));
            Some(function)
        })
    }

    unsafe fn required<F: Copy>(&mut self, what: &'static str, symbols: &[&'static CStr]) -> Result<F> {
        unsafe { self.optional(what, symbols) }.ok_or_else(|| err!("{what} is missing from the system library"))
    }
}

pub struct DisplayState {
    pub width: u32,
    pub height: u32,
    pub rotation: u8,
    pub refresh_rate: Option<f32>,
}

impl LibGui {
    pub fn get() -> Result<&'static LibGui> {
        static INSTANCE: OnceLock<std::result::Result<LibGui, String>> = OnceLock::new();
        INSTANCE.get_or_init(|| unsafe { Self::load() }.map_err(|error| error.to_string())).as_ref().map_err(|message| err!("{message}"))
    }

    unsafe fn load() -> Result<Self> {
        let api = utils::api_level();
        let utils = Library::open(c"libutils.so")?;
        let gui = Library::open(c"libgui.so")?;
        let mut utils = Resolver { library: &utils, picks: Vec::new() };
        let mut gui = Resolver { library: &gui, picks: Vec::new() };

        unsafe {
            let retain = utils.required("RefBase::incStrong", &[c"_ZNK7android7RefBase9incStrongEPKv"])?;
            let release = utils.required("RefBase::decStrong", &[c"_ZNK7android7RefBase9decStrongEPKv"])?;
            let string8_new = utils.required("String8::String8", &[c"_ZN7android7String8C1EPKc"])?;
            let string8_drop = utils.required("String8::~String8", &[c"_ZN7android7String8D1Ev"])?;

            let client_default = gui.optional("SurfaceComposerClient::getDefault", &[c"_ZN7android21SurfaceComposerClient10getDefaultEv"]);
            let client_new = gui.optional("SurfaceComposerClient::SurfaceComposerClient", &[c"_ZN7android21SurfaceComposerClientC1Ev"]);
            if client_default.is_none() && client_new.is_none() {
                bail!("SurfaceComposerClient cannot be constructed on this system");
            }
            let client_check = gui.optional("SurfaceComposerClient::initCheck", &[c"_ZNK7android21SurfaceComposerClient9initCheckEv"]);

            let create = CREATE
                .iter()
                .find_map(|&(symbol, kind)| {
                    let address = gui.library.find(symbol)?;
                    gui.picks.push(("SurfaceComposerClient::createSurface", symbol));
                    Some((address.as_ptr(), kind))
                })
                .ok_or_else(|| err!("no known SurfaceComposerClient::createSurface variant (API {api})"))?;

            let metadata_new = gui.optional("LayerMetadata::LayerMetadata", &[c"_ZN7android3gui13LayerMetadataC1Ev", c"_ZN7android13LayerMetadataC1Ev"]);
            let metadata_set = gui.optional("LayerMetadata::setInt32", &[c"_ZN7android3gui13LayerMetadata8setInt32Eji", c"_ZN7android13LayerMetadata8setInt32Eji"]);
            let needs_metadata = matches!(create.1, Create::Metadata | Create::MetadataHint(_) | Create::CheckedMetadata | Create::CheckedMetadataHint(_));
            if needs_metadata && metadata_new.is_none() {
                bail!("LayerMetadata constructor is missing (API {api})");
            }

            let validate = gui.optional("SurfaceControl::validate", &[c"_ZNK7android14SurfaceControl8validateEv"]);
            let surface = gui.required("SurfaceControl::getSurface", &[c"_ZN7android14SurfaceControl10getSurfaceEv", c"_ZNK7android14SurfaceControl10getSurfaceEv"])?;
            let disconnect = gui.optional("SurfaceControl::disconnect", &[c"_ZN7android14SurfaceControl10disconnectEv"]);
            let update_size = gui.optional("SurfaceControl::updateDefaultBufferSize", &[c"_ZN7android14SurfaceControl23updateDefaultBufferSizeEjj"]);

            let token = Self::resolve_token(&mut gui)?;
            let state = match gui
                .optional("SurfaceComposerClient::getDisplayState", &[c"_ZN7android21SurfaceComposerClient15getDisplayStateERKNS_2spINS_7IBinderEEEPNS_2ui12DisplayStateE"])
            {
                Some(query) => State::Modern(query),
                None => State::Legacy(
                    gui.required("SurfaceComposerClient::getDisplayInfo", &[c"_ZN7android21SurfaceComposerClient14getDisplayInfoERKNS_2spINS_7IBinderEEEPNS_11DisplayInfoE"])?,
                ),
            };
            let ops = Self::resolve_ops(&mut gui)?;
            let mirroring = Self::resolve_mirroring(&mut gui).filter(|_| matches!(ops, Ops::Transact(_)));

            let mut picks = utils.picks;
            picks.append(&mut gui.picks);
            Ok(Self {
                api,
                picks,
                retain,
                release,
                string8_new,
                string8_drop,
                client_default,
                client_new,
                client_check,
                create,
                metadata_new,
                metadata_set,
                validate,
                surface,
                disconnect,
                update_size,
                token,
                state,
                ops,
                mirroring,
            })
        }
    }

    unsafe fn resolve_token(gui: &mut Resolver) -> Result<Token> {
        unsafe {
            let ids = gui.optional("SurfaceComposerClient::getPhysicalDisplayIds", &[c"_ZN7android21SurfaceComposerClient21getPhysicalDisplayIdsEv"]);
            if let Some(token) = gui.optional("SurfaceComposerClient::getInternalDisplayToken", &[c"_ZN7android21SurfaceComposerClient23getInternalDisplayTokenEv"]) {
                return Ok(Token::Internal { token, ids });
            }
            let token = gui.optional("SurfaceComposerClient::getPhysicalDisplayToken", &[c"_ZN7android21SurfaceComposerClient23getPhysicalDisplayTokenENS_17PhysicalDisplayIdE"]);
            if let (Some(ids), Some(token)) = (ids, token) {
                return Ok(Token::Physical { ids, token });
            }
            Ok(Token::BuiltIn(gui.required("SurfaceComposerClient::getBuiltInDisplay", &[c"_ZN7android21SurfaceComposerClient17getBuiltInDisplayEi"])?))
        }
    }

    unsafe fn resolve_mirroring(gui: &mut Resolver) -> Option<Mirroring> {
        unsafe {
            let copy = match gui.optional("SurfaceComposerClient::mirrorSurface", &[c"_ZN7android21SurfaceComposerClient13mirrorSurfaceEPNS_14SurfaceControlE"]) {
                Some(plain) => CopyLayer::Plain(plain),
                None => {
                    CopyLayer::Cropped(gui.optional("SurfaceComposerClient::mirrorSurface", &[c"_ZN7android21SurfaceComposerClient13mirrorSurfaceEPNS_14SurfaceControlES2_S2_"])?)
                }
            };
            Some(Mirroring {
                copy,
                set_stack: gui.optional(
                    "Transaction::setLayerStack",
                    &[c"_ZN7android21SurfaceComposerClient11Transaction13setLayerStackERKNS_2spINS_14SurfaceControlEEENS_2ui10LayerStackE"],
                )?,
                reparent: gui.optional("Transaction::reparent", &[c"_ZN7android21SurfaceComposerClient11Transaction8reparentERKNS_2spINS_14SurfaceControlEEES6_"])?,
                set_matrix: gui.optional("Transaction::setMatrix", &[c"_ZN7android21SurfaceComposerClient11Transaction9setMatrixERKNS_2spINS_14SurfaceControlEEEffff"])?,
                set_position: gui.optional("Transaction::setPosition", &[c"_ZN7android21SurfaceComposerClient11Transaction11setPositionERKNS_2spINS_14SurfaceControlEEEff"])?,
            })
        }
    }

    unsafe fn resolve_ops(gui: &mut Resolver) -> Result<Ops> {
        unsafe {
            let set_layer: Option<TxInt> = gui.optional("Transaction::setLayer", &[c"_ZN7android21SurfaceComposerClient11Transaction8setLayerERKNS_2spINS_14SurfaceControlEEEi"]);
            let Some(set_layer) = set_layer else {
                return Ok(Ops::Global(Global {
                    open: gui.required("SurfaceComposerClient::openGlobalTransaction", &[c"_ZN7android21SurfaceComposerClient21openGlobalTransactionEv"])?,
                    close: gui.required("SurfaceComposerClient::closeGlobalTransaction", &[c"_ZN7android21SurfaceComposerClient22closeGlobalTransactionEb"])?,
                    set_layer: gui.required("SurfaceControl::setLayer", &[c"_ZN7android14SurfaceControl8setLayerEi", c"_ZN7android14SurfaceControl8setLayerEj"])?,
                    show: gui.required("SurfaceControl::show", &[c"_ZN7android14SurfaceControl4showEv"])?,
                    hide: gui.required("SurfaceControl::hide", &[c"_ZN7android14SurfaceControl4hideEv"])?,
                    set_size: gui.optional("SurfaceControl::setSize", &[c"_ZN7android14SurfaceControl7setSizeEjj"]),
                }));
            };

            let new = if let Some(create) = Library::open(c"libandroid.so").ok().and_then(|android| android.func(c"ASurfaceTransaction_create")) {
                NewTransaction::Ndk(create)
            } else if let Some(default) = gui.optional("Transaction::Transaction", &[c"_ZN7android21SurfaceComposerClient11TransactionC1Ev"]) {
                NewTransaction::Default(default)
            } else {
                NewTransaction::Copy(gui.required("Transaction::Transaction(const Transaction&)", &[c"_ZN7android21SurfaceComposerClient11TransactionC1ERKS1_"])?)
            };

            let apply = match gui.optional("Transaction::apply", &[c"_ZN7android21SurfaceComposerClient11Transaction5applyEbb"]) {
                Some(apply) => Apply::SyncOneWay(apply),
                None => Apply::Sync(gui.required("Transaction::apply", &[c"_ZN7android21SurfaceComposerClient11Transaction5applyEb"])?),
            };

            let trusted =
                match gui.optional("Transaction::setTrustedOverlay", &[c"_ZN7android21SurfaceComposerClient11Transaction17setTrustedOverlayERKNS_2spINS_14SurfaceControlEEEb"]) {
                    Some(set) => Some(Trusted::Bool(set)),
                    None => gui
                        .optional(
                            "Transaction::setTrustedOverlay",
                            &[c"_ZN7android21SurfaceComposerClient11Transaction17setTrustedOverlayERKNS_2spINS_14SurfaceControlEEENS_3gui14TrustedOverlayE"],
                        )
                        .map(Trusted::Enum),
                };

            Ok(Ops::Transact(Transact {
                new,
                set_layer,
                show: gui.required("Transaction::show", &[c"_ZN7android21SurfaceComposerClient11Transaction4showERKNS_2spINS_14SurfaceControlEEE"])?,
                hide: gui.required("Transaction::hide", &[c"_ZN7android21SurfaceComposerClient11Transaction4hideERKNS_2spINS_14SurfaceControlEEE"])?,
                set_size: gui.optional("Transaction::setSize", &[c"_ZN7android21SurfaceComposerClient11Transaction7setSizeERKNS_2spINS_14SurfaceControlEEEjj"]),
                trusted,
                apply,
            }))
        }
    }

    pub fn api(&self) -> u32 {
        self.api
    }

    pub fn picks(&self) -> &[(&'static str, &'static CStr)] {
        &self.picks
    }

    pub fn retain(&self, object: Ptr) {
        unsafe { (self.retain)(object, object) }
    }

    pub fn release(&self, object: Ptr) {
        unsafe { (self.release)(object, object) }
    }

    pub fn connect(&self) -> Result<Ptr> {
        start_binder_threads();
        let mut client = match self.client_default {
            Some(default) => unsafe { default() }.value,
            None => null_mut(),
        };
        if client.is_null() {
            let construct = self.client_new.ok_or_else(|| err!("SurfaceComposerClient::getDefault returned null"))?;
            client = construct_in_place("SurfaceComposerClient", |object| unsafe { construct(object) })?;
            self.retain(client);
        }
        if let Some(check) = self.client_check {
            let status = unsafe { check(client) };
            if status != 0 {
                bail!("SurfaceFlinger connection failed (status {status})");
            }
        }
        Ok(client)
    }

    /// The main display's token and, where the system has such a thing, its physical id.
    pub fn display_token(&self) -> Result<(Ptr, Option<u64>)> {
        let (token, id) = unsafe {
            match self.token {
                Token::Internal { token, ids } => (token().value, ids.and_then(|ids| main_display(ids))),
                Token::BuiltIn(get) => (get(0).value, None),
                Token::Physical { ids, token } => {
                    let Some(id) = main_display(ids) else { bail!("SurfaceFlinger reported no physical displays") };
                    (token(id).value, Some(id))
                }
            }
        };
        if token.is_null() {
            bail!("SurfaceFlinger returned no display token");
        }
        Ok((token, id))
    }

    pub fn display_state(&self, token: Ptr) -> Result<DisplayState> {
        #[repr(C, align(8))]
        struct Buffer([u8; 512]);
        let mut buffer = Buffer([0; 512]);
        let data = buffer.0.as_mut_ptr();
        let read = |offset: usize| unsafe { data.add(offset).cast::<u32>().read() };

        match self.state {
            State::Modern(query) => {
                let status = unsafe { query(&token, data) };
                if status != 0 {
                    bail!("getDisplayState failed (status {status})");
                }
                Ok(DisplayState { width: read(8), height: read(12), rotation: (read(4) & 3) as u8, refresh_rate: None })
            }
            State::Legacy(query) => {
                let status = unsafe { query(&token, data) };
                if status != 0 {
                    bail!("getDisplayInfo failed (status {status})");
                }
                let rotation = unsafe { data.add(24).read() } & 3;
                let (width, height) = (read(0), read(4));
                let (width, height) = if rotation & 1 == 1 { (height, width) } else { (width, height) };
                let refresh_rate = Some(f32::from_bits(read(16))).filter(|rate| rate.is_finite() && *rate >= 1.0);
                Ok(DisplayState { width, height, rotation, refresh_rate })
            }
        }
    }

    pub fn new_transaction(&self) -> Result<Ptr> {
        let Ops::Transact(transact) = &self.ops else {
            return Ok(null_mut());
        };
        match transact.new {
            NewTransaction::Ndk(create) => {
                let transaction = unsafe { create() };
                if transaction.is_null() {
                    bail!("ASurfaceTransaction_create returned null");
                }
                Ok(transaction)
            }
            NewTransaction::Default(construct) => construct_in_place("Transaction", |object| unsafe { construct(object) }),
            NewTransaction::Copy(construct) => construct_in_place("Transaction", |object| unsafe { construct(object, object) }),
        }
    }

    /// Creates a layer with buffers to draw into, or a bare container for other layers.
    pub fn create_surface(&self, client: Ptr, name: &CStr, width: u32, height: u32, capture: Capture, container: bool) -> Result<Ptr> {
        let (function, kind) = self.create;
        let hidden = capture == Capture::Hidden;
        let mut flags = if capture == Capture::Secure { FLAG_SECURE } else { 0 } | if container { FLAG_CONTAINER } else { 0 };
        let mut metadata = Storage::<512>::new();
        let mut name8 = Storage::<64>::new();

        let window_type = |default: i32| if hidden { WINDOW_TYPE_DONT_SCREENSHOT } else { default };
        let by_metadata = matches!(kind, Create::Metadata | Create::CheckedMetadata | Create::MetadataHint(Parent::Control) | Create::CheckedMetadataHint(Parent::Control));
        let by_flag = matches!(kind, Create::MetadataHint(Parent::Handle) | Create::CheckedMetadataHint(Parent::Handle));

        unsafe {
            if let Some(construct) = self.metadata_new {
                construct(metadata.ptr());
            }
            if hidden && by_metadata {
                match self.metadata_set {
                    Some(set) => set(metadata.ptr(), METADATA_WINDOW_TYPE, WINDOW_TYPE_DONT_SCREENSHOT),
                    None => log::warn!("capture hiding is unavailable: LayerMetadata::setInt32 is missing"),
                }
            }
            if hidden && by_flag {
                flags |= FLAG_SKIP_SCREENSHOT;
            }
            // Android 8.0 (API 26) already takes the window type that hides a layer, and does nothing with it.
            if hidden && self.api < 27 {
                log::warn!("a layer cannot be kept out of captures before Android 8.1");
            }

            (self.string8_new)(name8.ptr(), name.as_ptr());
            let name = name8.ptr();
            let no_handle: Ptr = null_mut();
            let parent = |parent: Parent| match parent {
                Parent::Control => null_mut(),
                Parent::Handle => (&raw const no_handle).cast_mut().cast::<c_void>(),
            };
            let mut control: Ptr = null_mut();
            let out = &raw mut control;
            let meta = metadata.ptr();
            let (w, h, format) = (width, height, FORMAT_RGBA_8888);

            let status = match kind {
                Create::Plain => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, u32) -> Returned<Ptr> = cast(function);
                    control = call(client, name, w, h, format, flags).value;
                    0
                }
                Create::WindowType(default) => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, u32, Ptr, i32, i32) -> Returned<Ptr> = cast(function);
                    control = call(client, name, w, h, format, flags, null_mut(), window_type(default), default).value;
                    0
                }
                Create::Metadata => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, u32, Ptr, Ptr) -> Returned<Ptr> = cast(function);
                    control = call(client, name, w, h, format, flags, null_mut(), meta).value;
                    0
                }
                Create::MetadataHint(kind) => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, u32, Ptr, Ptr, *mut u32) -> Returned<Ptr> = cast(function);
                    control = call(client, name, w, h, format, flags, parent(kind), meta, null_mut()).value;
                    0
                }
                Create::CheckedWindowType => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, *mut Ptr, u32, Ptr, i32, i32) -> i32 = cast(function);
                    call(client, name, w, h, format, out, flags, null_mut(), window_type(-1), -1)
                }
                Create::CheckedMetadata => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, *mut Ptr, u32, Ptr, Ptr) -> i32 = cast(function);
                    call(client, name, w, h, format, out, flags, null_mut(), meta)
                }
                Create::CheckedMetadataHint(kind) => {
                    let call: unsafe extern "C" fn(Ptr, Ptr, u32, u32, i32, *mut Ptr, u32, Ptr, Ptr, *mut u32) -> i32 = cast(function);
                    call(client, name, w, h, format, out, flags, parent(kind), meta, null_mut())
                }
            };
            (self.string8_drop)(name);

            if status != 0 || control.is_null() {
                bail!("SurfaceFlinger refused to create a {width}x{height} layer (status {status})");
            }
            if let Some(validate) = self.validate {
                let status = validate(control);
                if status != 0 {
                    self.release(control);
                    bail!("created layer is not valid (status {status})");
                }
            }
            Ok(control)
        }
    }

    pub fn window(&self, control: Ptr) -> Result<(Ptr, ANativeWindow)> {
        let surface = unsafe { (self.surface)(control) }.value;
        if surface.is_null() {
            bail!("SurfaceControl::getSurface returned null");
        }
        match unsafe { ANativeWindow::from_surface(surface) } {
            Ok(window) => Ok((surface, window)),
            Err(error) => {
                self.release(surface);
                Err(error)
            }
        }
    }

    pub fn disconnect(&self, control: Ptr) {
        if let Some(disconnect) = self.disconnect {
            unsafe { disconnect(control) };
        }
    }

    pub fn present(&self, transaction: Ptr, control: Ptr, z: i32) -> Result<()> {
        unsafe {
            match &self.ops {
                Ops::Transact(transact) => {
                    (transact.set_layer)(transaction, &control, z);
                    match transact.trusted {
                        Some(Trusted::Bool(set)) => {
                            set(transaction, &control, true);
                        }
                        Some(Trusted::Enum(set)) => {
                            set(transaction, &control, TRUSTED_OVERLAY_ENABLED);
                        }
                        None => {}
                    }
                    (transact.show)(transaction, &control);
                    self.apply(transact, transaction)
                }
                Ops::Global(global) => {
                    (global.open)();
                    (global.set_layer)(control, z);
                    (global.show)(control);
                    (global.close)(false);
                    Ok(())
                }
            }
        }
    }

    pub fn set_visible(&self, transaction: Ptr, control: Ptr, visible: bool) -> Result<()> {
        unsafe {
            match &self.ops {
                Ops::Transact(transact) => {
                    let change = if visible { transact.show } else { transact.hide };
                    change(transaction, &control);
                    self.apply(transact, transaction)
                }
                Ops::Global(global) => {
                    (global.open)();
                    if visible {
                        (global.show)(control)
                    } else {
                        (global.hide)(control)
                    };
                    (global.close)(false);
                    Ok(())
                }
            }
        }
    }

    pub fn can_mirror(&self) -> bool {
        self.mirroring.is_some()
    }

    /// Shows a copy of `layer` on the display with the given layer stack, inside a container
    /// that [`LibGui::place`] can scale and move. Returns the copy and its container.
    pub fn mirror(&self, client: Ptr, transaction: Ptr, layer: Ptr, [width, height]: [u32; 2], stack: u32) -> Result<[Handle; 2]> {
        let (Ops::Transact(transact), Some(mirroring)) = (&self.ops, &self.mirroring) else { bail!("layers cannot be mirrored on this system") };
        unsafe {
            let copy = match mirroring.copy {
                CopyLayer::Plain(mirror) => mirror(client, layer).value,
                CopyLayer::Cropped(mirror) => mirror(client, layer, null_mut(), null_mut()).value,
            };
            if copy.is_null() {
                bail!("SurfaceFlinger refused to mirror the layer");
            }
            let copy = Handle::adopt(copy);
            let root = Handle::adopt(self.create_surface(client, c"android-egui mirror", width, height, Capture::Visible, true)?);
            let (inner, outer) = (copy.as_ptr(), root.as_ptr());
            (transact.set_layer)(transaction, &outer, i32::MAX);
            (mirroring.set_stack)(transaction, &outer, stack);
            (transact.show)(transaction, &outer);
            (mirroring.set_stack)(transaction, &inner, stack);
            (transact.show)(transaction, &inner);
            (mirroring.reparent)(transaction, &inner, &outer);
            self.apply(transact, transaction)?;
            Ok([copy, root])
        }
    }

    /// Scales a layer and moves it within its display.
    pub fn place(&self, transaction: Ptr, layer: Ptr, scale: f32, [x, y]: [f32; 2]) -> Result<()> {
        let (Ops::Transact(transact), Some(mirroring)) = (&self.ops, &self.mirroring) else { bail!("layers cannot be placed on this system") };
        unsafe {
            (mirroring.set_matrix)(transaction, &layer, scale, 0.0, 0.0, scale);
            (mirroring.set_position)(transaction, &layer, x, y);
            self.apply(transact, transaction)
        }
    }

    /// Returns once SurfaceFlinger has taken the new size: a window surface made before that
    /// would still be given the old one by some EGL implementations.
    pub fn resize(&self, transaction: Ptr, control: Ptr, width: u32, height: u32) -> Result<bool> {
        unsafe {
            if let Some(update) = self.update_size {
                update(control, width, height);
                return Ok(true);
            }
            match &self.ops {
                Ops::Transact(transact) => match transact.set_size {
                    Some(set_size) => {
                        set_size(transaction, &control, width, height);
                        self.apply_now(transact, transaction, true).map(|()| true)
                    }
                    None => Ok(false),
                },
                Ops::Global(global) => match global.set_size {
                    Some(set_size) => {
                        (global.open)();
                        set_size(control, width, height);
                        (global.close)(true);
                        Ok(true)
                    }
                    None => Ok(false),
                },
            }
        }
    }

    unsafe fn apply(&self, transact: &Transact, transaction: Ptr) -> Result<()> {
        unsafe { self.apply_now(transact, transaction, false) }
    }

    /// With `wait`, returns only after SurfaceFlinger has applied the transaction.
    unsafe fn apply_now(&self, transact: &Transact, transaction: Ptr, wait: bool) -> Result<()> {
        let status = unsafe {
            match transact.apply {
                Apply::Sync(apply) => apply(transaction, wait),
                Apply::SyncOneWay(apply) => apply(transaction, wait, false),
            }
        };
        if status != 0 {
            bail!("SurfaceFlinger rejected the transaction (status {status})");
        }
        Ok(())
    }
}

unsafe fn cast<F: Copy>(function: Ptr) -> F {
    assert_eq!(size_of::<F>(), size_of::<Ptr>());
    unsafe { std::mem::transmute_copy(&function) }
}

fn construct_in_place(what: &str, construct: impl FnOnce(Ptr)) -> Result<Ptr> {
    let layout = Layout::from_size_align(OBJECT_SIZE + GUARD.len(), 16).map_err(|error| err!("{error}"))?;
    let object = unsafe { alloc_zeroed(layout) };
    if object.is_null() {
        bail!("out of memory");
    }
    let guard = unsafe { std::slice::from_raw_parts_mut(object.add(OBJECT_SIZE), GUARD.len()) };
    guard.copy_from_slice(&GUARD);
    construct(object.cast());
    if guard != GUARD {
        bail!("{what} is larger than expected on this system");
    }
    Ok(object.cast())
}

fn start_binder_threads() {
    let Ok(binder) = Library::open(c"libbinder_ndk.so") else {
        return;
    };
    if let Some(start) = unsafe { binder.func::<unsafe extern "C" fn()>(c"ABinderProcess_startThreadPool") } {
        unsafe { start() };
    }
}
