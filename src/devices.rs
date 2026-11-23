//! Windows audio endpoints: listing them, enabling or disabling one, and choosing the default device.
//!
//! Listing uses the documented MMDevice API. Enabling an endpoint and setting the default device go
//! through `IPolicyConfig`, the interface the Windows sound settings themselves use. It is not
//! documented, so its layout is taken as observed on Windows 10 and 11.

use crate::com::{Com, Error, Guid, Ole, Result, check, wide};
use std::ffi::c_void;

const CLSID_ENUMERATOR: Guid = Guid { d1: 0xBCDE_0395, d2: 0xE52F, d3: 0x467C, d4: [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E] };
const IID_ENUMERATOR: Guid = Guid { d1: 0xA956_64D2, d2: 0x9614, d3: 0x4F35, d4: [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6] };
const CLSID_POLICY_CONFIG: Guid = Guid { d1: 0x870A_F99C, d2: 0x171D, d3: 0x4F9E, d4: [0xAF, 0x0D, 0xE6, 0x3D, 0xF4, 0x0C, 0x2B, 0xC9] };
const IID_POLICY_CONFIG: Guid = Guid { d1: 0xF867_9F50, d2: 0x850A, d3: 0x41CF, d4: [0x9C, 0x72, 0x43, 0x0F, 0x29, 0x02, 0x90, 0xC8] };
const IID_SESSION_MANAGER: Guid = Guid { d1: 0x77AA_99A0, d2: 0x1BD6, d3: 0x484F, d4: [0x8B, 0xC7, 0x2C, 0x65, 0x4C, 0x9A, 0x9B, 0x6F] };
const IID_SESSION_CONTROL2: Guid = Guid { d1: 0xBFB7_FF88, d2: 0x7239, d3: 0x4FC9, d4: [0x8F, 0xA2, 0x07, 0xC9, 0x50, 0xBE, 0x9C, 0x6D] };

#[repr(C)]
struct PropertyKey {
    format: Guid,
    id: u32,
}

const DEVICE_GUID: Guid = Guid { d1: 0xA45C_254E, d2: 0xDF1C, d3: 0x4EFD, d4: [0x80, 0x20, 0x67, 0xD1, 0x46, 0xA8, 0x50, 0xE0] };
/// The full name Windows shows: "<description> (<interface name>)".
const FRIENDLY_NAME: PropertyKey = PropertyKey { format: DEVICE_GUID, id: 14 };
/// The endpoint's own name, the part before the parentheses ("Casque", "SteelSeries Sonar - Gaming").
const DEVICE_DESCRIPTION: PropertyKey = PropertyKey { format: DEVICE_GUID, id: 2 };
/// The name of the device behind the endpoint, the part in parentheses.
const INTERFACE_NAME: PropertyKey = PropertyKey { format: Guid { d1: 0xB3F8_FA53, d2: 0x0004, d3: 0x438E, d4: [0x90, 0x03, 0x51, 0xA4, 0x6E, 0x13, 0x9B, 0xFC] }, id: 6 };
/// The kernel-streaming interface path behind an endpoint ("{2}.\\?\usb#…", "{2}.\\?\bthhfenum#…").
const INTERFACE_PATH: PropertyKey = PropertyKey { format: Guid { d1: 0x2331_64C8, d2: 0x1B2C, d3: 0x4C7D, d4: [0xBC, 0x68, 0xB6, 0x71, 0x68, 0x7A, 0x25, 0x67] }, id: 1 };
/// The role the virtual audio driver gives each of its endpoints (`PKEY_SS_VAD_ROLE` in its INF).
/// It identifies a channel whatever the endpoint is called.
const VIRTUAL_ROLE: PropertyKey = PropertyKey { format: Guid { d1: 0xF141_E7CC, d2: 0x921B, d3: 0x466D, d4: [0x80, 0x2F, 0x87, 0x9D, 0x6D, 0x4A, 0xEC, 0x17] }, id: 0 };

/// The three names an endpoint carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameField {
    /// The endpoint's own name, before the parentheses.
    Description,
    /// The device's name, in parentheses.
    Interface,
    /// Both together, as Windows shows them.
    Friendly,
}

impl NameField {
    pub const ALL: [NameField; 3] = [NameField::Description, NameField::Interface, NameField::Friendly];

    fn key(self) -> &'static PropertyKey {
        match self {
            NameField::Description => &DEVICE_DESCRIPTION,
            NameField::Interface => &INTERFACE_NAME,
            NameField::Friendly => &FRIENDLY_NAME,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            NameField::Description => "description",
            NameField::Interface => "interface",
            NameField::Friendly => "friendly",
        }
    }

    pub fn parse(text: &str) -> Option<NameField> {
        NameField::ALL.into_iter().find(|field| field.label() == text)
    }
}

/// PROPVARIANT on 64-bit Windows: a type tag, padding, then 16 bytes of payload.
#[repr(C)]
struct PropVariant {
    kind: u16,
    reserved: [u16; 3],
    data: [usize; 2],
}

const VT_I2: u16 = 2;
const VT_I4: u16 = 3;
const VT_UI1: u16 = 17;
const VT_UI2: u16 = 18;
const VT_UI4: u16 = 19;
const VT_LPWSTR: u16 = 31;
/// IMMDevice::OpenPropertyStore modes.
const STGM_READ: u32 = 0;
const STGM_READWRITE: u32 = 2;
/// HRESULT for "element not found": no default device for that role, or no endpoint with that
/// identifier.
pub const NOT_FOUND: i32 = 0x8007_0490_u32 as i32;
/// IMMEndpoint, which tells an endpoint's direction.
const IID_ENDPOINT: Guid = Guid { d1: 0x1BE0_9788, d2: 0x6894, d3: 0x4089, d4: [0x85, 0x86, 0x9A, 0x2A, 0x6C, 0x26, 0x5A, 0xC5] };
const ENDPOINT_DATA_FLOW: usize = 3;

// Vtable positions (IUnknown takes entries 0 to 2).
const ENUMERATOR_ENUM: usize = 3;
const ENUMERATOR_DEFAULT: usize = 4;
const ENUMERATOR_DEVICE: usize = 5;
const DEVICE_ACTIVATE: usize = 3;
const COLLECTION_COUNT: usize = 3;
const COLLECTION_ITEM: usize = 4;
const DEVICE_PROPERTIES: usize = 4;
const DEVICE_ID: usize = 5;
const DEVICE_STATE: usize = 6;
const PROPERTIES_GET: usize = 5;
const PROPERTIES_SET: usize = 6;
const PROPERTIES_COMMIT: usize = 7;
const POLICY_SET_DEFAULT: usize = 3 + 10;
const POLICY_SET_VISIBLE: usize = 3 + 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Render = 0,
    Capture = 1,
}

impl Flow {
    pub const ALL: [Flow; 2] = [Flow::Render, Flow::Capture];

    pub fn label(self) -> &'static str {
        match self {
            Flow::Render => "render",
            Flow::Capture => "capture",
        }
    }

    pub fn parse(text: &str) -> Option<Flow> {
        match text {
            "render" | "output" => Some(Flow::Render),
            "capture" | "input" => Some(Flow::Capture),
            _ => None,
        }
    }
}

/// What a default device is the default for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Console = 0,
    Multimedia = 1,
    Communications = 2,
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Console, Role::Multimedia, Role::Communications];

    pub fn label(self) -> &'static str {
        match self {
            Role::Console => "console",
            Role::Multimedia => "multimedia",
            Role::Communications => "communications",
        }
    }

    pub fn parse(text: &str) -> Option<Role> {
        Role::ALL.into_iter().find(|role| role.label() == text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    Disabled,
    Unplugged,
    Other,
}

impl State {
    fn from_mask(mask: u32) -> State {
        match mask {
            1 => State::Active,
            2 => State::Disabled,
            8 => State::Unplugged,
            _ => State::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Disabled => "disabled",
            State::Unplugged => "unplugged",
            State::Other => "absent",
        }
    }
}

/// An application's audio session on an endpoint.
#[derive(Clone, Copy, Debug)]
pub struct Session {
    /// Process that owns the session; 0 for the system sounds.
    pub pid: u32,
    /// Whether one of its streams is running right now.
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub flow: Flow,
    pub state: State,
    /// For an endpoint of the virtual audio driver, the role it gives it (1 chat, 2 game,
    /// 3 microphone, 4 stream, 5 media, 6 aux); `None` for every other endpoint.
    pub virtual_role: Option<u32>,
}

/// A property value as far as this program needs to read one.
enum Property {
    Text(String),
    Number(u32),
    Other,
}

pub struct Devices {
    enumerator: Com,
    policy: Com,
    ole: Ole,
}

impl Devices {
    pub fn open() -> Result<Devices> {
        let ole = Ole::load()?;
        let enumerator = ole.create(&CLSID_ENUMERATOR, &IID_ENUMERATOR, "creating the audio device enumerator")?;
        let policy = ole.create(&CLSID_POLICY_CONFIG, &IID_POLICY_CONFIG, "creating the audio policy client")?;
        Ok(Devices { enumerator, policy, ole })
    }

    /// Every endpoint that is active, disabled or unplugged, outputs first.
    pub fn list(&self) -> Result<Vec<Endpoint>> {
        let mut endpoints = Vec::new();
        for flow in Flow::ALL {
            unsafe {
                let enumerate: unsafe extern "system" fn(*mut c_void, i32, u32, *mut *mut c_void) -> i32 = std::mem::transmute(self.enumerator.entry(ENUMERATOR_ENUM));
                let mut raw = std::ptr::null_mut();
                // 1 | 2 | 8 = active, disabled, unplugged
                check("IMMDeviceEnumerator::EnumAudioEndpoints", enumerate(self.enumerator.0, flow as i32, 1 | 2 | 8, &mut raw))?;
                let collection = Com(raw);

                let count: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(collection.entry(COLLECTION_COUNT));
                let mut n = 0;
                check("IMMDeviceCollection::GetCount", count(collection.0, &mut n))?;
                let item: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = std::mem::transmute(collection.entry(COLLECTION_ITEM));
                for index in 0..n {
                    let mut raw = std::ptr::null_mut();
                    check("IMMDeviceCollection::Item", item(collection.0, index, &mut raw))?;
                    endpoints.push(self.describe(&Com(raw), flow)?);
                }
            }
        }
        Ok(endpoints)
    }

    unsafe fn describe(&self, device: &Com, flow: Flow) -> Result<Endpoint> {
        unsafe {
            let get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32 = std::mem::transmute(device.entry(DEVICE_ID));
            let mut id = std::ptr::null_mut();
            check("IMMDevice::GetId", get_id(device.0, &mut id))?;
            let id = self.ole.take_wide(id);

            let get_state: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(device.entry(DEVICE_STATE));
            let mut state = 0;
            check("IMMDevice::GetState", get_state(device.0, &mut state))?;

            // A device whose properties cannot be read is still listed, under its identifier.
            let name = self.friendly_name(device).unwrap_or_else(|_| id.clone());
            let virtual_role = match self.property(device, &VIRTUAL_ROLE) {
                Ok(Property::Number(role)) => Some(role),
                _ => None,
            };
            Ok(Endpoint { id, name, flow, state: State::from_mask(state), virtual_role })
        }
    }

    /// One of the names of an endpoint; empty when it has none.
    pub fn name(&self, id: &str, field: NameField) -> Result<String> {
        let device = self.device(id)?;
        unsafe { self.text_property(&device, field.key()) }
    }

    /// Renames an endpoint, as the Windows sound settings do. Windows keeps the new name across
    /// restarts; it applies to every program that lists the device.
    pub fn set_name(&self, id: &str, field: NameField, text: &str) -> Result<()> {
        unsafe {
            let device = self.device(id)?;
            let open: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = std::mem::transmute(device.entry(DEVICE_PROPERTIES));
            let mut raw = std::ptr::null_mut();
            check("IMMDevice::OpenPropertyStore (read-write)", open(device.0, STGM_READWRITE, &mut raw))?;
            let properties = Com(raw);
            let text = wide(text);
            let value = PropVariant { kind: VT_LPWSTR, reserved: [0; 3], data: [text.as_ptr() as usize, 0] };
            let set: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *const PropVariant) -> i32 = std::mem::transmute(properties.entry(PROPERTIES_SET));
            check("IPropertyStore::SetValue", set(properties.0, field.key(), &value))?;
            let commit: unsafe extern "system" fn(*mut c_void) -> i32 = std::mem::transmute(properties.entry(PROPERTIES_COMMIT));
            check("IPropertyStore::Commit", commit(properties.0))
        }
    }

    unsafe fn friendly_name(&self, device: &Com) -> Result<String> {
        unsafe {
            let name = self.text_property(device, &FRIENDLY_NAME)?;
            if name.is_empty() { Err(Error::Invalid("the device has no friendly name".into())) } else { Ok(name) }
        }
    }

    /// A text property of a device; empty when it holds something else.
    unsafe fn text_property(&self, device: &Com, key: &PropertyKey) -> Result<String> {
        unsafe {
            Ok(match self.property(device, key)? {
                Property::Text(text) => text,
                _ => String::new(),
            })
        }
    }

    unsafe fn property(&self, device: &Com, key: &PropertyKey) -> Result<Property> {
        unsafe {
            let open: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = std::mem::transmute(device.entry(DEVICE_PROPERTIES));
            let mut raw = std::ptr::null_mut();
            check("IMMDevice::OpenPropertyStore", open(device.0, STGM_READ, &mut raw))?;
            let properties = Com(raw);

            let get: unsafe extern "system" fn(*mut c_void, *const PropertyKey, *mut PropVariant) -> i32 = std::mem::transmute(properties.entry(PROPERTIES_GET));
            let mut value = PropVariant { kind: 0, reserved: [0; 3], data: [0; 2] };
            check("IPropertyStore::GetValue", get(properties.0, key, &mut value))?;
            let payload = value.data[0];
            let property = match value.kind {
                VT_LPWSTR => Property::Text(crate::com::read_wide(payload as *const u16)),
                VT_UI1 => Property::Number(payload as u8 as u32),
                VT_I2 | VT_UI2 => Property::Number(payload as u16 as u32),
                VT_I4 | VT_UI4 => Property::Number(payload as u32),
                _ => Property::Other,
            };
            self.ole.clear_variant((&mut value as *mut PropVariant).cast());
            Ok(property)
        }
    }

    /// Whether an endpoint is the microphone of a Bluetooth headset, which switches the headset to
    /// its hands-free profile, degrading everything it plays, as soon as it is opened.
    pub fn is_hands_free(&self, id: &str) -> bool {
        self.device(id).and_then(|device| unsafe { self.text_property(&device, &INTERFACE_PATH) }).is_ok_and(|path| path.to_ascii_lowercase().contains("bthhfenum"))
    }

    /// Identifier of the default device for a role, if there is one.
    pub fn default(&self, flow: Flow, role: Role) -> Result<Option<String>> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, i32, i32, *mut *mut c_void) -> i32 = std::mem::transmute(self.enumerator.entry(ENUMERATOR_DEFAULT));
            let mut raw = std::ptr::null_mut();
            let hresult = get(self.enumerator.0, flow as i32, role as i32, &mut raw);
            if hresult == NOT_FOUND {
                return Ok(None);
            }
            check("IMMDeviceEnumerator::GetDefaultAudioEndpoint", hresult)?;
            let device = Com(raw);
            let get_id: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32 = std::mem::transmute(device.entry(DEVICE_ID));
            let mut id = std::ptr::null_mut();
            check("IMMDevice::GetId", get_id(device.0, &mut id))?;
            Ok(Some(self.ole.take_wide(id)))
        }
    }

    /// Obtains one of an endpoint's interfaces (an audio client, a peak meter). This opens no stream.
    pub fn activate(&self, id: &str, interface: &Guid, what: &'static str) -> Result<Com> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32 = std::mem::transmute(self.enumerator.entry(ENUMERATOR_DEVICE));
            let mut raw = std::ptr::null_mut();
            check("IMMDeviceEnumerator::GetDevice", get(self.enumerator.0, wide(id).as_ptr(), &mut raw))?;
            let device = Com(raw);

            let activate: unsafe extern "system" fn(*mut c_void, *const Guid, u32, *mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(device.entry(DEVICE_ACTIVATE));
            let mut raw = std::ptr::null_mut();
            // 23 = CLSCTX_ALL
            check(what, activate(device.0, interface, 23, std::ptr::null_mut(), &mut raw))?;
            Ok(Com(raw))
        }
    }

    /// The streams applications have on an endpoint: who plays to an output, who records from an input.
    pub fn sessions(&self, id: &str) -> Result<Vec<Session>> {
        const MANAGER_ENUMERATOR: usize = 5;
        const ENUMERATOR_COUNT: usize = 3;
        const ENUMERATOR_SESSION: usize = 4;
        const CONTROL_STATE: usize = 3;
        const CONTROL_PROCESS: usize = 14;
        unsafe {
            let manager = self.activate(id, &IID_SESSION_MANAGER, "activating the session manager")?;
            let enumerate: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(manager.entry(MANAGER_ENUMERATOR));
            let mut raw = std::ptr::null_mut();
            check("IAudioSessionManager2::GetSessionEnumerator", enumerate(manager.0, &mut raw))?;
            let enumerator = Com(raw);

            let count: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(enumerator.entry(ENUMERATOR_COUNT));
            let mut n = 0;
            check("IAudioSessionEnumerator::GetCount", count(enumerator.0, &mut n))?;
            let get: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32 = std::mem::transmute(enumerator.entry(ENUMERATOR_SESSION));
            let mut sessions = Vec::new();
            for index in 0..n {
                let mut raw = std::ptr::null_mut();
                check("IAudioSessionEnumerator::GetSession", get(enumerator.0, index, &mut raw))?;
                let control = Com(raw).query(&IID_SESSION_CONTROL2, "getting the session details")?;

                let get_state: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(control.entry(CONTROL_STATE));
                let mut state = 0;
                check("IAudioSessionControl::GetState", get_state(control.0, &mut state))?;
                let get_process: unsafe extern "system" fn(*mut c_void, *mut u32) -> i32 = std::mem::transmute(control.entry(CONTROL_PROCESS));
                let mut pid = 0;
                // A session shared by several processes reports a failure code along with one of them.
                get_process(control.0, &mut pid);
                // 1 = a stream of the session is running
                sessions.push(Session { pid, active: state == 1 });
            }
            Ok(sessions)
        }
    }

    /// One endpoint, found by its identifier, as `list` describes it: cheaper than listing them all
    /// when only one matters. Fails with `NOT_FOUND` for an identifier Windows does not know.
    pub fn endpoint(&self, id: &str) -> Result<Endpoint> {
        unsafe {
            let device = self.device(id)?;
            let endpoint = device.query(&IID_ENDPOINT, "getting the endpoint's direction")?;
            let get_flow: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(endpoint.entry(ENDPOINT_DATA_FLOW));
            let mut flow = 0;
            check("IMMEndpoint::GetDataFlow", get_flow(endpoint.0, &mut flow))?;
            self.describe(&device, if flow == Flow::Capture as i32 { Flow::Capture } else { Flow::Render })
        }
    }

    /// The IMMDevice of an endpoint. Nothing is opened.
    pub fn device(&self, id: &str) -> Result<Com> {
        unsafe {
            let get: unsafe extern "system" fn(*mut c_void, *const u16, *mut *mut c_void) -> i32 = std::mem::transmute(self.enumerator.entry(ENUMERATOR_DEVICE));
            let mut raw = std::ptr::null_mut();
            check("IMMDeviceEnumerator::GetDevice", get(self.enumerator.0, wide(id).as_ptr(), &mut raw))?;
            Ok(Com(raw))
        }
    }

    /// The read-only property store of an endpoint, as Windows hands it to the effects of that endpoint.
    pub fn properties(&self, id: &str) -> Result<Com> {
        unsafe {
            let device = self.device(id)?;
            let open: unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> i32 = std::mem::transmute(device.entry(DEVICE_PROPERTIES));
            let mut raw = std::ptr::null_mut();
            check("IMMDevice::OpenPropertyStore", open(device.0, STGM_READ, &mut raw))?;
            Ok(Com(raw))
        }
    }

    /// The COM runtime helpers, for callers that receive COM-allocated memory.
    pub fn ole(&self) -> &Ole {
        &self.ole
    }

    /// Enables or disables an endpoint, as the Windows sound settings do.
    pub fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32 = std::mem::transmute(self.policy.entry(POLICY_SET_VISIBLE));
            check("IPolicyConfig::SetEndpointVisibility", call(self.policy.0, wide(id).as_ptr(), i32::from(enabled)))
        }
    }

    /// Makes an endpoint the default device for one role.
    pub fn set_default(&self, id: &str, role: Role) -> Result<()> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *const u16, i32) -> i32 = std::mem::transmute(self.policy.entry(POLICY_SET_DEFAULT));
            check("IPolicyConfig::SetDefaultEndpoint", call(self.policy.0, wide(id).as_ptr(), role as i32))
        }
    }
}

/// Finds one device from an identifier or part of a name, optionally prefixed with `render:` or `capture:`.
pub fn find_device<'a>(endpoints: &'a [Endpoint], spec: &str) -> std::result::Result<&'a Endpoint, String> {
    if let Some(exact) = endpoints.iter().find(|e| e.id.eq_ignore_ascii_case(spec)) {
        return Ok(exact);
    }
    let (flow, needle) = match spec.split_once(':') {
        Some((prefix, rest)) if Flow::parse(prefix).is_some() => (Flow::parse(prefix), rest),
        _ => (None, spec),
    };
    let needle = needle.to_lowercase();
    let matches: Vec<&Endpoint> = endpoints.iter().filter(|e| flow.is_none_or(|f| f == e.flow) && e.name.to_lowercase().contains(&needle)).collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no audio device matches \"{spec}\"")),
        several => {
            let names: Vec<String> = several.iter().map(|e| format!("  {}:{}", e.flow.label(), e.name)).collect();
            Err(format!("\"{spec}\" matches several devices:\n{}", names.join("\n")))
        }
    }
}
