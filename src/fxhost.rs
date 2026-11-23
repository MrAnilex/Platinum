//! Hosting an audio processing object (APO) outside the Windows audio engine.
//!
//! An APO is the COM object the Windows audio engine loads to apply effects to an audio endpoint.
//! A program can also host one itself and feed it a stream it captured, which is what the vendor's
//! service does with its microphone effects. This module does the same through the documented APO
//! interfaces (audioenginebaseapo.h): it loads the DLL, creates the object from its class
//! identifier, initialises it, agrees on a format with it, and calls its processing function block
//! by block on buffers this module owns.
//!
//! The host-side objects an APO expects (format descriptions, property stores, a device collection)
//! are small COM objects implemented here. Layouts and constants follow analysis/sonar/native/
//! api-spec.md and the way the vendor's own host drives the same APO (host-library.md).

use crate::com::{Com, Error, Guid, Ole, Result, check, export, load_library, wide};
use crate::wasapi::{Format, WaveFormat, read_format};
use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

pub const IID_UNKNOWN: Guid = Guid { d1: 0, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
const IID_CLASS_FACTORY: Guid = Guid { d1: 1, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };
pub const IID_APO: Guid = Guid { d1: 0xFD7F_2B29, d2: 0x24D0, d3: 0x4B5C, d4: [0xB1, 0x77, 0x59, 0x2C, 0x39, 0xF9, 0xCA, 0x10] };
pub const IID_APO_RT: Guid = Guid { d1: 0x9E1D_6A6D, d2: 0xDDBC, d3: 0x4E95, d4: [0xA4, 0xC7, 0xAD, 0x64, 0xBA, 0x37, 0x84, 0x6C] };
pub const IID_APO_CONFIGURATION: Guid = Guid { d1: 0x0E5E_D805, d2: 0xABA6, d3: 0x49C3, d4: [0x8F, 0x9A, 0x2B, 0x8C, 0x88, 0x9C, 0x4F, 0xA8] };
const IID_MEDIA_TYPE: Guid = Guid { d1: 0x4E99_7F73, d2: 0xB71F, d3: 0x4798, d4: [0x87, 0x3B, 0xED, 0x7D, 0xFC, 0xF1, 0x5B, 0x4D] };
const IID_PROPERTY_STORE: Guid = Guid { d1: 0x886D_8EEB, d2: 0x8CF2, d3: 0x4446, d4: [0x8D, 0x02, 0xCD, 0xBA, 0x1D, 0xBD, 0xCF, 0x99] };
const IID_DEVICE_COLLECTION: Guid = Guid { d1: 0x0BD7_A1BE, d2: 0x7A1A, d3: 0x44DB, d4: [0x83, 0x97, 0xCC, 0x53, 0x92, 0x38, 0x7B, 0x5E] };
const SUBTYPE_FLOAT: Guid = Guid { d1: 0x0000_0003, d2: 0x0000, d3: 0x0010, d4: [0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71] };
/// AUDIO_SIGNALPROCESSINGMODE_DEFAULT.
pub const MODE_DEFAULT: Guid = Guid { d1: 0xC18E_2F7E, d2: 0x933D, d3: 0x4965, d4: [0xB7, 0xD1, 0x1E, 0xEF, 0x22, 0x8D, 0x2A, 0xF3] };

// Vtable positions (IUnknown takes entries 0 to 2).
const APO_GET_LATENCY: usize = 4;
const APO_GET_REGISTRATION: usize = 5;
const APO_INITIALIZE: usize = 6;
const APO_IS_INPUT_SUPPORTED: usize = 7;
const APO_IS_OUTPUT_SUPPORTED: usize = 8;
const RT_PROCESS: usize = 3;
const CONFIG_LOCK: usize = 3;
const CONFIG_UNLOCK: usize = 4;
const MEDIA_TYPE_GET_AUDIO_FORMAT: usize = 5;
const STORE_GET_COUNT: usize = 3;
const STORE_GET_AT: usize = 4;
const STORE_GET_VALUE: usize = 5;

/// APO_CONNECTION_DESCRIPTOR_SIGNATURE ('ACDS'); the APO rejects a descriptor without it.
const DESCRIPTOR_SIGNATURE: u32 = 0x4143_4453;
/// APO_CONNECTION_PROPERTY_SIGNATURE ('ACPS').
const PROPERTY_SIGNATURE: u32 = 0x4143_5053;
/// APO_CONNECTION_BUFFER_TYPE_ALLOCATED, with no buffer in the descriptor: what the vendor's host does.
const BUFFER_TYPE_ALLOCATED: i32 = 0;
const BUFFER_VALID: u32 = 1;
const BUFFER_SILENT: u32 = 2;
const EQUAL_FORMAT_TYPES: u32 = 0x2;
const EQUAL_FORMAT_DATA: u32 = 0x4;
const EQUAL_FORMAT_USER_DATA: u32 = 0x8;

const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
const E_POINTER: i32 = 0x8000_4003_u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
const STG_E_ACCESSDENIED: i32 = 0x8003_0005_u32 as i32;
const S_FALSE: i32 = 1;

const VT_EMPTY: u16 = 0;
const VT_UI4: u16 = 19;
const VT_LPWSTR: u16 = 31;

/// APOInitBaseStruct.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitBase {
    pub size: u32,
    pub clsid: Guid,
}

/// APOInitSystemEffects2.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitSystemEffects2 {
    pub base: InitBase,
    pub endpoint_properties: *mut c_void,
    pub system_effects_properties: *mut c_void,
    pub reserved: *mut c_void,
    pub device_collection: *mut c_void,
    pub software_io_device: u32,
    pub software_io_connector: u32,
    pub processing_mode: Guid,
    pub discovery_only: i32,
}

const _: () = assert!(std::mem::size_of::<InitBase>() == 20);
const _: () = assert!(std::mem::size_of::<InitSystemEffects2>() == 0x58);
const _: () = assert!(std::mem::offset_of!(InitSystemEffects2, endpoint_properties) == 0x18);
const _: () = assert!(std::mem::offset_of!(InitSystemEffects2, device_collection) == 0x30);
const _: () = assert!(std::mem::offset_of!(InitSystemEffects2, software_io_device) == 0x38);
const _: () = assert!(std::mem::offset_of!(InitSystemEffects2, processing_mode) == 0x40);
const _: () = assert!(std::mem::offset_of!(InitSystemEffects2, discovery_only) == 0x50);

/// APO_CONNECTION_DESCRIPTOR.
#[repr(C)]
struct ConnectionDescriptor {
    kind: i32,
    buffer: usize,
    max_frames: u32,
    format: *mut c_void,
    signature: u32,
}

/// APO_CONNECTION_PROPERTY.
#[repr(C)]
#[derive(Clone, Copy)]
struct ConnectionProperty {
    buffer: usize,
    valid_frames: u32,
    flags: u32,
    signature: u32,
}

const _: () = assert!(std::mem::size_of::<ConnectionDescriptor>() == 0x28);
const _: () = assert!(std::mem::offset_of!(ConnectionDescriptor, format) == 0x18);
const _: () = assert!(std::mem::offset_of!(ConnectionDescriptor, signature) == 0x20);
const _: () = assert!(std::mem::size_of::<ConnectionProperty>() == 0x18);
const _: () = assert!(std::mem::offset_of!(ConnectionProperty, signature) == 0x10);

/// UNCOMPRESSEDAUDIOFORMAT.
#[repr(C)]
#[derive(Clone, Copy)]
struct UncompressedFormat {
    format_type: Guid,
    samples_per_frame: u32,
    bytes_per_container: u32,
    valid_bits: u32,
    frames_per_second: f32,
    channel_mask: u32,
}

const _: () = assert!(std::mem::size_of::<UncompressedFormat>() == 36);

/// PROPERTYKEY.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PropertyKey {
    pub format: Guid,
    pub id: u32,
}

/// PROPVARIANT on 64-bit Windows: a type tag, padding, then 16 bytes of payload.
#[repr(C)]
struct PropVariant {
    kind: u16,
    reserved: [u16; 3],
    data: [usize; 2],
}

const _: () = assert!(std::mem::size_of::<PropVariant>() == 24);

/// What the APO says about itself (APO_REG_PROPERTIES).
#[derive(Clone, Debug)]
pub struct Registration {
    pub clsid: Guid,
    pub flags: u32,
    pub name: String,
    pub copyright: String,
    pub version: (u32, u32),
    pub inputs: (u32, u32),
    pub outputs: (u32, u32),
    pub max_instances: u32,
    pub interfaces: Vec<Guid>,
}

// APO_REG_PROPERTIES offsets: the CLSID, the flags, two 256-character names, then 32-bit counts.
const REGISTRATION_NAME: usize = 20;
const REGISTRATION_COPYRIGHT: usize = 532;
const REGISTRATION_VERSION: usize = 1044;
const REGISTRATION_INTERFACE_COUNT: usize = 1072;
const REGISTRATION_INTERFACES: usize = 1076;

/// Reads an APO_REG_PROPERTIES structure from its bytes.
fn parse_registration(bytes: &[u8]) -> Registration {
    let u32_at = |offset: usize| u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    let guid_at = |offset: usize| Guid {
        d1: u32_at(offset),
        d2: u16::from_le_bytes([bytes[offset + 4], bytes[offset + 5]]),
        d3: u16::from_le_bytes([bytes[offset + 6], bytes[offset + 7]]),
        d4: bytes[offset + 8..offset + 16].try_into().unwrap(),
    };
    let text_at = |offset: usize| {
        let units: Vec<u16> = bytes[offset..offset + 512].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|u| *u != 0).collect();
        String::from_utf16_lossy(&units)
    };
    let count = (u32_at(REGISTRATION_INTERFACE_COUNT) as usize).min((bytes.len() - REGISTRATION_INTERFACES) / 16);
    Registration {
        clsid: guid_at(0),
        flags: u32_at(16),
        name: text_at(REGISTRATION_NAME),
        copyright: text_at(REGISTRATION_COPYRIGHT),
        version: (u32_at(REGISTRATION_VERSION), u32_at(REGISTRATION_VERSION + 4)),
        inputs: (u32_at(REGISTRATION_VERSION + 8), u32_at(REGISTRATION_VERSION + 12)),
        outputs: (u32_at(REGISTRATION_VERSION + 16), u32_at(REGISTRATION_VERSION + 20)),
        max_instances: u32_at(REGISTRATION_VERSION + 24),
        interfaces: (0..count).map(|i| guid_at(REGISTRATION_INTERFACES + 16 * i)).collect(),
    }
}

/// The APO's answer when asked whether it accepts a format.
#[derive(Debug)]
pub enum Negotiation {
    Accepted,
    /// Not this one, but this other format instead.
    Suggested(Format),
    Refused(i32),
}

/// CoTaskMemAlloc, resolved once: property values handed to the APO must come from the COM allocator.
fn task_alloc(bytes: usize) -> *mut c_void {
    static ALLOC: OnceLock<Option<unsafe extern "system" fn(usize) -> *mut c_void>> = OnceLock::new();
    let alloc = ALLOC.get_or_init(|| {
        let module = load_library("ole32.dll", 0).ok()?;
        export(module, b"CoTaskMemAlloc\0").ok().map(|address| unsafe { std::mem::transmute::<*mut c_void, unsafe extern "system" fn(usize) -> *mut c_void>(address) })
    });
    match alloc {
        Some(alloc) => unsafe { alloc(bytes) },
        None => std::ptr::null_mut(),
    }
}

// ---------------------------------------------------------------------------------------------
// IAudioMediaType: the APO asks the host's format objects what they describe.

#[repr(C)]
struct MediaTypeVtbl {
    query_interface: unsafe extern "system" fn(*mut MediaType, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut MediaType) -> u32,
    release: unsafe extern "system" fn(*mut MediaType) -> u32,
    is_compressed: unsafe extern "system" fn(*mut MediaType, *mut i32) -> i32,
    is_equal: unsafe extern "system" fn(*mut MediaType, *mut c_void, *mut u32) -> i32,
    audio_format: unsafe extern "system" fn(*mut MediaType) -> *const WaveFormat,
    uncompressed_format: unsafe extern "system" fn(*mut MediaType, *mut UncompressedFormat) -> i32,
}

#[repr(C)]
struct MediaType {
    vtable: &'static MediaTypeVtbl,
    refs: AtomicU32,
    format: Format,
    wave: WaveFormat,
    uncompressed: UncompressedFormat,
}

static MEDIA_TYPE_VTBL: MediaTypeVtbl = MediaTypeVtbl {
    query_interface: media_type_query,
    add_ref: media_type_add_ref,
    release: media_type_release,
    is_compressed: media_type_is_compressed,
    is_equal: media_type_is_equal,
    audio_format: media_type_audio_format,
    uncompressed_format: media_type_uncompressed,
};

/// A format object for the APO (32-bit float), holding one reference that the caller must release.
fn new_media_type(format: Format) -> *mut MediaType {
    // The description below is only true of 32-bit float, the one sample type APOs process.
    assert!(format.float && format.bits == 32, "an APO format must be 32-bit float, not {format}");
    let uncompressed = UncompressedFormat {
        format_type: SUBTYPE_FLOAT,
        samples_per_frame: u32::from(format.channels),
        bytes_per_container: u32::from(format.bits / 8),
        valid_bits: u32::from(format.bits),
        frames_per_second: format.rate as f32,
        channel_mask: format.channel_mask,
    };
    Box::into_raw(Box::new(MediaType { vtable: &MEDIA_TYPE_VTBL, refs: AtomicU32::new(1), format, wave: format.to_wave(), uncompressed }))
}

unsafe fn release_media_type(media_type: *mut MediaType) {
    unsafe {
        media_type_release(media_type);
    }
}

unsafe extern "system" fn media_type_query(this: *mut MediaType, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    unsafe {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        if *iid == IID_UNKNOWN || *iid == IID_MEDIA_TYPE {
            media_type_add_ref(this);
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn media_type_add_ref(this: *mut MediaType) -> u32 {
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn media_type_release(this: *mut MediaType) -> u32 {
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left
    }
}

unsafe extern "system" fn media_type_is_compressed(_this: *mut MediaType, compressed: *mut i32) -> i32 {
    unsafe {
        if compressed.is_null() {
            return E_POINTER;
        }
        *compressed = 0;
        0
    }
}

unsafe extern "system" fn media_type_is_equal(this: *mut MediaType, other: *mut c_void, flags: *mut u32) -> i32 {
    unsafe {
        if other.is_null() || flags.is_null() {
            return E_POINTER;
        }
        // Borrowed, not owned: the caller keeps its reference.
        let other = std::mem::ManuallyDrop::new(Com(other));
        let get: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(other.entry(MEDIA_TYPE_GET_AUDIO_FORMAT));
        let theirs = get(other.0);
        if theirs.is_null() {
            *flags = 0;
            return S_FALSE;
        }
        let (ours, theirs) = ((*this).format, read_format(theirs));
        let mut equal = 0;
        if ours.float == theirs.float {
            equal |= EQUAL_FORMAT_TYPES;
        }
        if ours == theirs {
            equal |= EQUAL_FORMAT_DATA | EQUAL_FORMAT_USER_DATA;
        }
        *flags = equal;
        if equal == EQUAL_FORMAT_TYPES | EQUAL_FORMAT_DATA | EQUAL_FORMAT_USER_DATA { 0 } else { S_FALSE }
    }
}

unsafe extern "system" fn media_type_audio_format(this: *mut MediaType) -> *const WaveFormat {
    unsafe { &raw const (*this).wave }
}

unsafe extern "system" fn media_type_uncompressed(this: *mut MediaType, out: *mut UncompressedFormat) -> i32 {
    unsafe {
        if out.is_null() {
            return E_POINTER;
        }
        *out = (*this).uncompressed;
        0
    }
}

// ---------------------------------------------------------------------------------------------
// IPropertyStore: what the APO reads about its endpoint. Read-only.

/// A value the host answers itself instead of asking a device.
#[derive(Clone, Debug)]
#[allow(dead_code)] // not needed by the microphone set-up, which reads real devices; kept for other APOs
pub enum Answer {
    Number(u32),
    Text(String),
    Empty,
}

#[repr(C)]
struct StoreVtbl {
    query_interface: unsafe extern "system" fn(*mut HostStore, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut HostStore) -> u32,
    release: unsafe extern "system" fn(*mut HostStore) -> u32,
    get_count: unsafe extern "system" fn(*mut HostStore, *mut u32) -> i32,
    get_at: unsafe extern "system" fn(*mut HostStore, u32, *mut PropertyKey) -> i32,
    get_value: unsafe extern "system" fn(*mut HostStore, *const PropertyKey, *mut PropVariant) -> i32,
    set_value: unsafe extern "system" fn(*mut HostStore, *const PropertyKey, *const PropVariant) -> i32,
    commit: unsafe extern "system" fn(*mut HostStore) -> i32,
}

#[repr(C)]
struct HostStore {
    vtable: &'static StoreVtbl,
    refs: AtomicU32,
    /// Keys answered from a given device store first (falling back to `fallback` when that store
    /// has no value), or answered directly.
    routes: Vec<(PropertyKey, Route)>,
    /// Where every other key is read; none means every other key is empty.
    fallback: Option<Com>,
}

enum Route {
    Store(Com),
    Answer(Answer),
}

static STORE_VTBL: StoreVtbl = StoreVtbl {
    query_interface: store_query,
    add_ref: store_add_ref,
    release: store_release,
    get_count: store_get_count,
    get_at: store_get_at,
    get_value: store_get_value,
    set_value: store_set_value,
    commit: store_commit,
};

/// Builds a read-only property store for an APO.
pub struct StoreBuilder {
    routes: Vec<(PropertyKey, Route)>,
    fallback: Option<Com>,
}

impl StoreBuilder {
    /// A store whose keys are all empty unless routed otherwise.
    pub fn new() -> StoreBuilder {
        StoreBuilder { routes: Vec::new(), fallback: None }
    }

    /// Reads every key not routed otherwise from this device store.
    pub fn fallback(mut self, store: Com) -> StoreBuilder {
        self.fallback = Some(store);
        self
    }

    /// Reads `key` from this device store first.
    pub fn route(mut self, key: PropertyKey, store: Com) -> StoreBuilder {
        self.routes.push((key, Route::Store(store)));
        self
    }

    /// Answers `key` directly.
    #[allow(dead_code)] // see `Answer`
    pub fn answer(mut self, key: PropertyKey, answer: Answer) -> StoreBuilder {
        self.routes.push((key, Route::Answer(answer)));
        self
    }

    /// The store as a COM object with one reference, owned by the returned value.
    pub fn build(self) -> Com {
        let store = Box::into_raw(Box::new(HostStore { vtable: &STORE_VTBL, refs: AtomicU32::new(1), routes: self.routes, fallback: self.fallback }));
        Com(store.cast())
    }
}

unsafe fn device_store_get(store: &Com, key: *const PropertyKey, value: *mut PropVariant) -> i32 {
    unsafe {
        let get: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32 = std::mem::transmute(store.entry(STORE_GET_VALUE));
        get(store.0, key, value)
    }
}

unsafe fn write_answer(answer: &Answer, value: *mut PropVariant) -> i32 {
    unsafe {
        *value = PropVariant { kind: VT_EMPTY, reserved: [0; 3], data: [0; 2] };
        match answer {
            Answer::Empty => {}
            Answer::Number(number) => {
                (*value).kind = VT_UI4;
                (*value).data[0] = *number as usize;
            }
            Answer::Text(text) => {
                let units = wide(text);
                let memory = task_alloc(units.len() * 2).cast::<u16>();
                if memory.is_null() {
                    return 0x8007_000E_u32 as i32; // E_OUTOFMEMORY
                }
                std::ptr::copy_nonoverlapping(units.as_ptr(), memory, units.len());
                (*value).kind = VT_LPWSTR;
                (*value).data[0] = memory as usize;
            }
        }
        0
    }
}

unsafe extern "system" fn store_query(this: *mut HostStore, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    unsafe {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        if *iid == IID_UNKNOWN || *iid == IID_PROPERTY_STORE {
            store_add_ref(this);
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn store_add_ref(this: *mut HostStore) -> u32 {
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn store_release(this: *mut HostStore) -> u32 {
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left
    }
}

unsafe extern "system" fn store_get_count(this: *mut HostStore, count: *mut u32) -> i32 {
    unsafe {
        if count.is_null() {
            return E_POINTER;
        }
        match &(*this).fallback {
            Some(store) => {
                let call: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(store.entry(STORE_GET_COUNT));
                call(store.0, count)
            }
            None => {
                *count = 0;
                0
            }
        }
    }
}

unsafe extern "system" fn store_get_at(this: *mut HostStore, index: u32, key: *mut PropertyKey) -> i32 {
    unsafe {
        if key.is_null() {
            return E_POINTER;
        }
        match &(*this).fallback {
            Some(store) => {
                let call: unsafe extern "system" fn(*mut c_void, u32, *mut PropertyKey) -> i32 = std::mem::transmute(store.entry(STORE_GET_AT));
                call(store.0, index, key)
            }
            None => E_INVALIDARG,
        }
    }
}

unsafe extern "system" fn store_get_value(this: *mut HostStore, key: *const PropertyKey, value: *mut PropVariant) -> i32 {
    unsafe {
        if key.is_null() || value.is_null() {
            return E_POINTER;
        }
        let empty = || PropVariant { kind: VT_EMPTY, reserved: [0; 3], data: [0; 2] };
        *value = empty();
        for (routed, route) in &(*this).routes {
            if *routed != *key {
                continue;
            }
            match route {
                Route::Answer(answer) => return write_answer(answer, value),
                Route::Store(store) => {
                    // Like the vendor's host: a routed store that fails or has no value falls back.
                    if device_store_get(store, key, value) >= 0 && (*value).kind != VT_EMPTY {
                        return 0;
                    }
                    *value = empty();
                }
            }
            break;
        }
        match &(*this).fallback {
            Some(store) => device_store_get(store, key, value),
            None => 0,
        }
    }
}

unsafe extern "system" fn store_set_value(_this: *mut HostStore, _key: *const PropertyKey, _value: *const PropVariant) -> i32 {
    STG_E_ACCESSDENIED
}

unsafe extern "system" fn store_commit(_this: *mut HostStore) -> i32 {
    STG_E_ACCESSDENIED
}

// ---------------------------------------------------------------------------------------------
// IMMDeviceCollection holding a single device: the endpoint the APO is told it works for.

#[repr(C)]
struct CollectionVtbl {
    query_interface: unsafe extern "system" fn(*mut OneDevice, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut OneDevice) -> u32,
    release: unsafe extern "system" fn(*mut OneDevice) -> u32,
    get_count: unsafe extern "system" fn(*mut OneDevice, *mut u32) -> i32,
    item: unsafe extern "system" fn(*mut OneDevice, u32, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct OneDevice {
    vtable: &'static CollectionVtbl,
    refs: AtomicU32,
    device: Com,
}

static COLLECTION_VTBL: CollectionVtbl = CollectionVtbl { query_interface: collection_query, add_ref: collection_add_ref, release: collection_release, get_count: collection_get_count, item: collection_item };

/// A device collection with one IMMDevice in it, as a COM object owned by the returned value.
pub fn single_device_collection(device: Com) -> Com {
    let collection = Box::into_raw(Box::new(OneDevice { vtable: &COLLECTION_VTBL, refs: AtomicU32::new(1), device }));
    Com(collection.cast())
}

unsafe extern "system" fn collection_query(this: *mut OneDevice, iid: *const Guid, out: *mut *mut c_void) -> i32 {
    unsafe {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        if *iid == IID_UNKNOWN || *iid == IID_DEVICE_COLLECTION {
            collection_add_ref(this);
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn collection_add_ref(this: *mut OneDevice) -> u32 {
    unsafe { (*this).refs.fetch_add(1, Ordering::AcqRel) + 1 }
}

unsafe extern "system" fn collection_release(this: *mut OneDevice) -> u32 {
    unsafe {
        let left = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            drop(Box::from_raw(this));
        }
        left
    }
}

unsafe extern "system" fn collection_get_count(_this: *mut OneDevice, count: *mut u32) -> i32 {
    unsafe {
        if count.is_null() {
            return E_POINTER;
        }
        *count = 1;
        0
    }
}

unsafe extern "system" fn collection_item(this: *mut OneDevice, index: u32, out: *mut *mut c_void) -> i32 {
    unsafe {
        if out.is_null() {
            return E_POINTER;
        }
        if index != 0 {
            *out = std::ptr::null_mut();
            return E_INVALIDARG;
        }
        // The caller receives its own reference, as COM requires.
        let device = &(*this).device;
        let add_ref: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(device.entry(1));
        add_ref(device.0);
        *out = device.0;
        0
    }
}

// ---------------------------------------------------------------------------------------------

#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetDllDirectoryW(path: *const u16) -> i32;
}

/// An APO created from its DLL, not yet locked for processing.
pub struct Engine {
    apo: Com,
    rt: Com,
    config: Com,
}

impl Engine {
    /// Loads `dll` and creates the APO `clsid` in this process, asking its factory for `interface`
    /// first, then for the standard APO interfaces. Nothing is initialised yet.
    ///
    /// The DLL's folder is added to the DLL search path for the rest of the process's life: the
    /// APO loads libraries from it at run time.
    pub fn create(dll: &Path, clsid: &Guid, interface: &Guid) -> Result<Engine> {
        let path = dll.to_str().ok_or_else(|| Error::Load("the DLL path is not valid Unicode".into()))?;
        if !dll.is_file() {
            return Err(Error::Load(format!("{path} not found: the effects engine does not seem to be installed")));
        }
        Ole::load()?;
        if let Some(folder) = dll.parent().and_then(Path::to_str) {
            unsafe {
                SetDllDirectoryW(wide(folder).as_ptr());
            }
        }
        // 0x8 = LOAD_WITH_ALTERED_SEARCH_PATH, so the DLL finds the libraries next to it.
        let module = load_library(path, 0x8)?;
        unsafe {
            let get_class_object: unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(export(module, b"DllGetClassObject\0")?);
            let mut raw = std::ptr::null_mut();
            check("DllGetClassObject", get_class_object(clsid, &IID_CLASS_FACTORY, &mut raw))?;
            let factory = Com(raw);
            let create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(factory.entry(3));
            let mut raw = std::ptr::null_mut();
            check("IClassFactory::CreateInstance", create(factory.0, std::ptr::null_mut(), interface, &mut raw))?;
            let object = Com(raw);
            Ok(Engine {
                apo: object.query(&IID_APO, "querying IAudioProcessingObject")?,
                rt: object.query(&IID_APO_RT, "querying IAudioProcessingObjectRT")?,
                config: object.query(&IID_APO_CONFIGURATION, "querying IAudioProcessingObjectConfiguration")?,
            })
        }
    }

    /// Whether the object answers to an interface.
    pub fn supports(&self, iid: &Guid) -> bool {
        self.apo.query(iid, "QueryInterface").is_ok()
    }

    pub fn registration(&self, ole: &Ole) -> Result<Registration> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *mut *mut u8) -> i32 = std::mem::transmute(self.apo.entry(APO_GET_REGISTRATION));
            let mut raw = std::ptr::null_mut();
            check("IAudioProcessingObject::GetRegistrationProperties", get(self.apo.0, &mut raw))?;
            if raw.is_null() {
                return Err(Error::Invalid("the APO returned no registration properties".into()));
            }
            // The structure ends with as many interface identifiers as it announces.
            let count = std::ptr::read_unaligned(raw.add(REGISTRATION_INTERFACE_COUNT).cast::<u32>()).min(64) as usize;
            let registration = parse_registration(std::slice::from_raw_parts(raw, REGISTRATION_INTERFACES + 16 * count.max(1)));
            ole.free(raw.cast());
            Ok(registration)
        }
    }

    /// Initialises the APO with `data`, a structure that begins with its own size.
    pub fn initialize<T: Copy>(&self, data: &T) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, u32, *const u8) -> i32 = std::mem::transmute(self.apo.entry(APO_INITIALIZE));
            check("IAudioProcessingObject::Initialize", call(self.apo.0, std::mem::size_of::<T>() as u32, (data as *const T).cast()))
        }
    }

    /// Delay the APO says it adds, in seconds.
    pub fn latency(&self) -> Result<f64> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut i64) -> i32 = std::mem::transmute(self.apo.entry(APO_GET_LATENCY));
            let mut time = 0i64;
            check("IAudioProcessingObject::GetLatency", call(self.apo.0, &mut time))?;
            // Units of 100 nanoseconds.
            Ok(time as f64 / 10_000_000.0)
        }
    }

    pub fn input_supported(&self, opposite: Option<Format>, requested: Format) -> Negotiation {
        self.negotiate(APO_IS_INPUT_SUPPORTED, opposite, requested)
    }

    pub fn output_supported(&self, opposite: Option<Format>, requested: Format) -> Negotiation {
        self.negotiate(APO_IS_OUTPUT_SUPPORTED, opposite, requested)
    }

    fn negotiate(&self, entry: usize, opposite: Option<Format>, requested: Format) -> Negotiation {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(self.apo.entry(entry));
            let opposite = opposite.map(new_media_type);
            let requested = new_media_type(requested);
            let mut supported = std::ptr::null_mut();
            let hresult = call(self.apo.0, opposite.map_or(std::ptr::null_mut(), |o| o.cast()), requested.cast(), &mut supported);
            let answer = if supported.is_null() {
                if hresult == 0 { Negotiation::Accepted } else { Negotiation::Refused(hresult) }
            } else {
                // The APO hands back an object it gave us a reference to: read it, then release it.
                let object = Com(supported);
                let get: unsafe extern "system" fn(*mut c_void) -> *const u8 = std::mem::transmute(object.entry(MEDIA_TYPE_GET_AUDIO_FORMAT));
                let format = get(object.0);
                match (hresult, format.is_null()) {
                    (0, _) => Negotiation::Accepted,
                    (S_FALSE, false) => Negotiation::Suggested(read_format(format)),
                    _ => Negotiation::Refused(hresult),
                }
            };
            release_media_type(requested);
            if let Some(opposite) = opposite {
                release_media_type(opposite);
            }
            answer
        }
    }

    /// Prepares the APO to process `format` (32-bit float) in blocks of up to `max_frames` frames.
    /// Input and output are separate buffers, as the vendor's host does.
    pub fn lock(self, format: Format, max_frames: usize) -> std::result::Result<Locked, (Engine, Error)> {
        let channels = usize::from(format.channels);
        let media_type = new_media_type(format);
        let mut descriptor = Box::new(ConnectionDescriptor { kind: BUFFER_TYPE_ALLOCATED, buffer: 0, max_frames: max_frames as u32, format: media_type.cast(), signature: DESCRIPTOR_SIGNATURE });
        let hresult = unsafe {
            let call: unsafe extern "system" fn(*mut c_void, u32, *mut *mut ConnectionDescriptor, u32, *mut *mut ConnectionDescriptor) -> i32 = std::mem::transmute(self.config.entry(CONFIG_LOCK));
            // The same descriptor for the input and the output, as the vendor's host does.
            let mut inputs = [&raw mut *descriptor];
            let mut outputs = [&raw mut *descriptor];
            call(self.config.0, 1, inputs.as_mut_ptr(), 1, outputs.as_mut_ptr())
        };
        if let Err(error) = check("IAudioProcessingObjectConfiguration::LockForProcess", hresult) {
            unsafe { release_media_type(media_type) };
            return Err((self, error));
        }
        Ok(Locked {
            engine: self,
            format,
            channels,
            max_frames,
            input: vec![0.0; max_frames * channels].into_boxed_slice(),
            output: vec![0.0; max_frames * channels].into_boxed_slice(),
            media_type,
            _descriptor: descriptor,
            properties: Box::new([ConnectionProperty { buffer: 0, valid_frames: 0, flags: BUFFER_VALID, signature: PROPERTY_SIGNATURE }; 2]),
        })
    }
}

/// An APO locked for processing, with the buffers it is given.
pub struct Locked {
    engine: Engine,
    format: Format,
    channels: usize,
    max_frames: usize,
    input: Box<[f32]>,
    output: Box<[f32]>,
    media_type: *mut MediaType,
    _descriptor: Box<ConnectionDescriptor>,
    properties: Box<[ConnectionProperty; 2]>,
}

impl Locked {
    /// The format the APO was locked for.
    pub fn format(&self) -> Format {
        self.format
    }

    /// Runs one block through the APO: `samples` (interleaved, at most the locked number of frames)
    /// are replaced by the processed audio. Returns false when the APO marked its output as silence.
    pub fn process(&mut self, samples: &mut [f32]) -> bool {
        let frames = (samples.len() / self.channels).min(self.max_frames);
        let len = frames * self.channels;
        self.input[..len].copy_from_slice(&samples[..len]);
        // Flags are set again on every call: the APO copies the input's flags to the output.
        self.properties[0] = ConnectionProperty { buffer: self.input.as_mut_ptr() as usize, valid_frames: frames as u32, flags: BUFFER_VALID, signature: PROPERTY_SIGNATURE };
        self.properties[1] = ConnectionProperty { buffer: self.output.as_mut_ptr() as usize, valid_frames: frames as u32, flags: BUFFER_VALID, signature: PROPERTY_SIGNATURE };
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, u32, *mut *mut ConnectionProperty, u32, *mut *mut ConnectionProperty) = std::mem::transmute(self.engine.rt.entry(RT_PROCESS));
            let mut inputs = [&raw mut self.properties[0]];
            let mut outputs = [&raw mut self.properties[1]];
            call(self.engine.rt.0, 1, inputs.as_mut_ptr(), 1, outputs.as_mut_ptr());
        }
        let silent = self.properties[1].flags == BUFFER_SILENT;
        if silent {
            samples[..len].fill(0.0);
        } else {
            samples[..len].copy_from_slice(&self.output[..len]);
        }
        !silent
    }
}

impl Drop for Locked {
    fn drop(&mut self) {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(self.engine.config.entry(CONFIG_UNLOCK));
            call(self.engine.config.0);
            // The APO took its own reference to the format if it kept it.
            release_media_type(self.media_type);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo() -> Format {
        Format { rate: 48_000, channels: 2, channel_mask: 0x3, bits: 32, float: true }
    }

    /// Function pointer stored at `index` in a COM object's vtable, as an APO would call it.
    unsafe fn entry<T>(object: *mut T, index: usize) -> *const c_void {
        unsafe { *(*(object as *const *const *const c_void)).add(index) }
    }

    #[test]
    fn media_type_answers_like_a_com_object() {
        unsafe {
            let object = new_media_type(stereo());
            let query: unsafe extern "system" fn(*mut MediaType, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(entry(object, 0));
            let mut out = std::ptr::null_mut();
            assert_eq!(query(object, &IID_MEDIA_TYPE, &mut out), 0);
            assert_eq!(out, object.cast());
            assert_eq!(query(object, &IID_APO, &mut out), E_NOINTERFACE);
            assert!(out.is_null());
            assert_eq!((*object).refs.load(Ordering::SeqCst), 2);

            let format: unsafe extern "system" fn(*mut MediaType) -> *const u8 = std::mem::transmute(entry(object, MEDIA_TYPE_GET_AUDIO_FORMAT));
            assert_eq!(read_format(format(object)), stereo());

            let uncompressed: unsafe extern "system" fn(*mut MediaType, *mut UncompressedFormat) -> i32 = std::mem::transmute(entry(object, 6));
            let mut details: UncompressedFormat = std::mem::zeroed();
            assert_eq!(uncompressed(object, &mut details), 0);
            assert!(details.format_type == SUBTYPE_FLOAT && details.samples_per_frame == 2 && details.bytes_per_container == 4 && details.valid_bits == 32);
            assert!(details.frames_per_second == 48_000.0 && details.channel_mask == 0x3);

            let mono = new_media_type(Format { channels: 1, channel_mask: 0x4, ..stereo() });
            let same = new_media_type(stereo());
            let is_equal: unsafe extern "system" fn(*mut MediaType, *mut c_void, *mut u32) -> i32 = std::mem::transmute(entry(object, 4));
            let mut flags = 0;
            assert_eq!(is_equal(object, same.cast(), &mut flags), 0);
            assert_eq!(flags, EQUAL_FORMAT_TYPES | EQUAL_FORMAT_DATA | EQUAL_FORMAT_USER_DATA);
            assert_eq!(is_equal(object, mono.cast(), &mut flags), S_FALSE);
            assert_eq!(flags, EQUAL_FORMAT_TYPES);

            for item in [object, object, mono, same] {
                release_media_type(item);
            }
        }
    }

    #[test]
    fn host_store_routes_answers_and_stays_read_only() {
        let role = PropertyKey { format: Guid { d1: 0xF141_E7CC, d2: 0x921B, d3: 0x466D, d4: [0x80, 0x2F, 0x87, 0x9D, 0x6D, 0x4A, 0xEC, 0x17] }, id: 0 };
        let repository = PropertyKey { id: 1, ..role };
        let other = PropertyKey { id: 7, ..role };
        let inner = StoreBuilder::new().answer(repository, Answer::Text("ChatCapture".into())).build();
        let store = StoreBuilder::new().answer(role, Answer::Number(3)).route(repository, inner).build();
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32 = std::mem::transmute(store.entry(STORE_GET_VALUE));
            let mut value = PropVariant { kind: 99, reserved: [0; 3], data: [7, 7] };
            assert_eq!(get(store.0, &role, &mut value), 0);
            assert_eq!((value.kind, value.data[0]), (VT_UI4, 3));
            assert_eq!(get(store.0, &repository, &mut value), 0);
            assert_eq!(value.kind, VT_LPWSTR);
            assert_eq!(crate::com::read_wide(value.data[0] as *const u16), "ChatCapture");
            // Freed the way the APO frees it: the string came from the COM allocator.
            let ole = Ole::load().unwrap();
            ole.clear_variant((&raw mut value).cast());
            assert_eq!(get(store.0, &other, &mut value), 0);
            assert_eq!(value.kind, VT_EMPTY);
            let set: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *const PropVariant) -> i32 = std::mem::transmute(store.entry(6));
            assert_eq!(set(store.0, &role, &value), STG_E_ACCESSDENIED);
            let count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(store.entry(STORE_GET_COUNT));
            let mut n = 9;
            assert_eq!(count(store.0, &mut n), 0);
            assert_eq!(n, 0);
        }
    }

    #[test]
    fn registration_is_read_at_the_documented_offsets() {
        let mut bytes = vec![0u8; REGISTRATION_INTERFACES + 32];
        let clsid = Guid { d1: 0x4B77_58C2, d2: 0x9361, d3: 0x4F88, d4: [0x83, 0xE9, 0x44, 0xD8, 0xF0, 0x51, 0x84, 0xEF] };
        let put_guid = |bytes: &mut Vec<u8>, offset: usize, g: &Guid| {
            bytes[offset..offset + 4].copy_from_slice(&g.d1.to_le_bytes());
            bytes[offset + 4..offset + 6].copy_from_slice(&g.d2.to_le_bytes());
            bytes[offset + 6..offset + 8].copy_from_slice(&g.d3.to_le_bytes());
            bytes[offset + 8..offset + 16].copy_from_slice(&g.d4);
        };
        put_guid(&mut bytes, 0, &clsid);
        bytes[16..20].copy_from_slice(&0xFu32.to_le_bytes());
        for (i, unit) in "Mic".encode_utf16().enumerate() {
            bytes[REGISTRATION_NAME + 2 * i..REGISTRATION_NAME + 2 * i + 2].copy_from_slice(&unit.to_le_bytes());
        }
        for (i, value) in [1u32, 2, 1, 1, 1, 1, 0xFFFF_FFFF, 2].iter().enumerate() {
            bytes[REGISTRATION_VERSION + 4 * i..REGISTRATION_VERSION + 4 * i + 4].copy_from_slice(&value.to_le_bytes());
        }
        put_guid(&mut bytes, REGISTRATION_INTERFACES, &IID_APO);
        put_guid(&mut bytes, REGISTRATION_INTERFACES + 16, &IID_APO_RT);
        let registration = parse_registration(&bytes);
        assert_eq!(registration.clsid, clsid);
        assert_eq!(registration.flags, 0xF);
        assert_eq!(registration.name, "Mic");
        assert_eq!(registration.copyright, "");
        assert_eq!((registration.version, registration.inputs, registration.outputs, registration.max_instances), ((1, 2), (1, 1), (1, 1), 0xFFFF_FFFF));
        assert_eq!(registration.interfaces, vec![IID_APO, IID_APO_RT]);
    }
}
