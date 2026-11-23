//! Windows backend: talks to the audio processing module that is already installed on the machine,
//! through the settings API exported by its companion DLL.
//!
//! The DLL is loaded directly and its class factory is obtained from `DllGetClassObject`, so nothing
//! is read from or written to the COM registry. Each channel has its own control object and its own
//! settings store. Stores are shared between processes and outlive the process that wrote to them.
//!
//! The interfaces are dual (IUnknown + IDispatch + methods), so method `n` of an interface sits at
//! vtable entry `7 + n`.

use crate::com::{Com, Error, Guid, Ole, Result, check, export, load_library};
use std::ffi::c_void;
use std::fmt;
use std::path::Path;

/// Where GG installs the settings API. Without GG, `sonarfiles::api_dll` finds the copy of the
/// installed driver package.
pub const DEFAULT_DLL: &str = r"C:\Program Files\SteelSeries\GG\apps\sonar\Sonar.APOAPI.dll";

const CLSID_BASIC_CONTROL: Guid = Guid { d1: 0xDEA0_5346, d2: 0x7BE3, d3: 0x4DB0, d4: [0xAE, 0x9F, 0x14, 0x42, 0x36, 0x48, 0xEA, 0x7B] };
const IID_BASIC_CONTROL: Guid = Guid { d1: 0x4007_6C40, d2: 0x0E9C, d3: 0x4D14, d4: [0x9C, 0xF6, 0x34, 0x83, 0x9A, 0x76, 0x6B, 0x8A] };
const IID_CLASS_FACTORY: Guid = Guid { d1: 0x0000_0001, d2: 0, d3: 0, d4: [0xC0, 0, 0, 0, 0, 0, 0, 0x46] };

/// First vtable entry after IUnknown (3 entries) and IDispatch (4 entries).
const DUAL: usize = 7;

// Vtable positions, counted from `DUAL`.
const CONTROL_OPEN_STORE: usize = DUAL + 13;
const CONTROL_INITIALIZE: usize = DUAL + 34;
const STORE_COUNT: usize = DUAL;
const STORE_AT: usize = DUAL + 1;
const STORE_OPEN: usize = DUAL + 2;
const SETTING_ID: usize = DUAL;
const SETTING_KIND: usize = DUAL + 1;
const SETTING_NAME: usize = DUAL + 6;
const SETTING_LONG_TRAITS: usize = DUAL + 8;
const SETTING_FLOAT_TRAITS: usize = DUAL + 9;
const SETTING_GET_BOOL: usize = DUAL + 10;
const SETTING_GET_LONG: usize = DUAL + 11;
const SETTING_GET_FLOAT: usize = DUAL + 12;
const SETTING_GET_STRING: usize = DUAL + 13;
const SETTING_SET_BOOL: usize = DUAL + 18;
const SETTING_SET_LONG: usize = DUAL + 19;
const SETTING_SET_FLOAT: usize = DUAL + 20;
const SETTING_SET_STRING: usize = DUAL + 21;

/// The few string helpers needed from oleaut32, resolved at run time.
struct Strings {
    alloc: unsafe extern "system" fn(*const u16, u32) -> *mut u16,
    free: unsafe extern "system" fn(*mut u16),
}

impl Strings {
    fn load() -> Result<Strings> {
        let module = load_library("oleaut32.dll", 0)?;
        unsafe {
            Ok(Strings {
                alloc: std::mem::transmute(export(module, b"SysAllocStringLen\0")?),
                free: std::mem::transmute(export(module, b"SysFreeString\0")?),
            })
        }
    }

    /// Calls `f` with a temporary BSTR holding `text`.
    fn with<T>(&self, text: &str, f: impl FnOnce(*mut u16) -> T) -> T {
        let units: Vec<u16> = text.encode_utf16().collect();
        let bstr = unsafe { (self.alloc)(units.as_ptr(), units.len() as u32) };
        let result = f(bstr);
        unsafe { (self.free)(bstr) };
        result
    }

    /// Takes ownership of a BSTR returned by the API.
    fn take(&self, bstr: *mut u16) -> String {
        unsafe {
            let text = crate::com::read_wide(bstr);
            (self.free)(bstr);
            text
        }
    }
}

/// The five mixer channels. Each one is a separate settings store in the audio module.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    Game,
    Chat,
    Mic,
    Media,
    Aux,
}

impl Channel {
    pub const ALL: [Channel; 5] = [Channel::Game, Channel::Chat, Channel::Media, Channel::Aux, Channel::Mic];

    /// Name the audio module knows this channel by.
    fn repository(self) -> &'static str {
        match self {
            Channel::Game => "Game",
            Channel::Chat => "ChatRender",
            Channel::Mic => "ChatCapture",
            Channel::Media => "Media",
            Channel::Aux => "Aux",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Channel::Game => "game",
            Channel::Chat => "chat",
            Channel::Mic => "mic",
            Channel::Media => "media",
            Channel::Aux => "aux",
        }
    }

    pub fn is_capture(self) -> bool {
        self == Channel::Mic
    }

    /// The registry key, under HKEY_LOCAL_MACHINE, whose `kSet_…` values hold this channel's
    /// settings: the effects engine reads them there, and the settings API writes them there (the
    /// microphone's echo references were found there on 17/11/2026). Its last-write time tells
    /// when a setting may have changed, without opening a store.
    pub fn registry_key(self) -> String {
        format!(r"SOFTWARE\SteelSeries ApS\Sonar.APO\{}\Settings\GlobalControl\Store", self.repository())
    }

    pub fn parse(text: &str) -> Option<Channel> {
        match text.to_ascii_lowercase().as_str() {
            "game" | "gaming" => Some(Channel::Game),
            "chat" | "chatrender" => Some(Channel::Chat),
            "mic" | "microphone" | "chatcapture" => Some(Channel::Mic),
            "media" => Some(Channel::Media),
            "aux" => Some(Channel::Aux),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i32),
    Float(f32),
    Text(String),
    /// Array-typed settings are not read yet; the number is the module's type code.
    Unsupported(i32),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{}", (f64::from(*v) * 10_000.0).round() / 10_000.0),
            Value::Text(v) => write!(f, "\"{v}\""),
            Value::Unsupported(code) => write!(f, "(array, type {code})"),
        }
    }
}

/// The loaded settings API.
pub struct Apo {
    factory: Com,
    strings: Strings,
}

impl Apo {
    pub fn load(dll: &Path) -> Result<Apo> {
        let path = dll.to_str().ok_or_else(|| Error::Load("the DLL path is not valid Unicode".into()))?;
        if !dll.is_file() {
            return Err(Error::Load(format!("{path} not found: the audio module does not seem to be installed")));
        }
        let strings = Strings::load()?;
        // The module's own code expects COM to be initialised on the calling thread.
        Ole::load()?;

        // 0x8 = LOAD_WITH_ALTERED_SEARCH_PATH, so the DLL finds its neighbours.
        let module = load_library(path, 0x8)?;
        let get_class_object: unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> i32 =
            unsafe { std::mem::transmute(export(module, b"DllGetClassObject\0")?) };
        let mut factory = std::ptr::null_mut();
        check("DllGetClassObject", unsafe { get_class_object(&CLSID_BASIC_CONTROL, &IID_CLASS_FACTORY, &mut factory) })?;
        Ok(Apo { factory: Com(factory), strings })
    }

    /// Opens the settings store of one channel.
    pub fn open(&self, channel: Channel) -> Result<Store<'_>> {
        unsafe {
            let create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32 = std::mem::transmute(self.factory.entry(3));
            let mut raw = std::ptr::null_mut();
            check("IClassFactory::CreateInstance", create(self.factory.0, std::ptr::null_mut(), &IID_BASIC_CONTROL, &mut raw))?;
            let control = Com(raw);

            let initialize: unsafe extern "system" fn(*mut c_void, *mut u16) -> i32 = std::mem::transmute(control.entry(CONTROL_INITIALIZE));
            check("IBasicControl::Initialize", self.strings.with(channel.repository(), |id| initialize(control.0, id)))?;

            let open_store: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> i32 = std::mem::transmute(control.entry(CONTROL_OPEN_STORE));
            let mut raw = std::ptr::null_mut();
            check("IBasicControl::OpenStore", open_store(control.0, &mut raw))?;
            Ok(Store { store: Com(raw), _control: control, apo: self, channel })
        }
    }
}

/// A store that failed to open is tried again after this long: every attempt costs the vendor's
/// DLL memory.
pub const STORE_RETRY: std::time::Duration = std::time::Duration::from_secs(30);

/// Every channel's settings store, each opened once and kept for the life of the process. The
/// vendor's DLL leaks memory and handles on every opening of a store (a window that reopened five
/// of them every 2 s grew past 200 MB), so a long-running program must not open them again.
pub struct Stores {
    /// The settings API, loaded once and never unloaded (the stores borrow it).
    api: std::result::Result<&'static Apo, String>,
    /// In `Channel::ALL` order.
    slots: [Option<Store<'static>>; 5],
    /// When each store last failed to open.
    failed: [Option<std::time::Instant>; 5],
}

impl Stores {
    /// Loads the settings API; opens no store yet.
    pub fn load(dll: &Path) -> Stores {
        let api = Apo::load(dll).map(|apo| &*Box::leak(Box::new(apo))).map_err(|e| e.to_string());
        Stores { api, slots: Default::default(), failed: [None; 5] }
    }

    /// The installed API (`sonarfiles::api_dll`: `PLATINUM_APO_DLL`, GG's folder, the driver
    /// package, Platinum's copy).
    pub fn load_default() -> Stores {
        Stores::load(&crate::sonarfiles::api_dll().0)
    }

    /// Why the settings API could not be loaded, if it could not.
    pub fn api_error(&self) -> Option<&str> {
        self.api.as_ref().err().map(String::as_str)
    }

    fn index(channel: Channel) -> usize {
        Channel::ALL.iter().position(|c| *c == channel).unwrap_or(0)
    }

    /// Opens the stores not opened yet, at most once every `STORE_RETRY` for one that failed.
    pub fn open_all(&mut self) {
        for channel in Channel::ALL {
            self.get(channel);
        }
    }

    /// A channel's store, opened now if it was not (at most once every `STORE_RETRY` after a
    /// failure); `None` while it cannot be opened.
    pub fn get(&mut self, channel: Channel) -> Option<&Store<'static>> {
        let index = Stores::index(channel);
        if self.slots[index].is_none() && !self.failed[index].is_some_and(|when| when.elapsed() < STORE_RETRY) {
            if let Ok(apo) = self.api {
                match apo.open(channel) {
                    Ok(store) => {
                        self.slots[index] = Some(store);
                        self.failed[index] = None;
                    }
                    Err(_) => self.failed[index] = Some(std::time::Instant::now()),
                }
            }
        }
        self.slots[index].as_ref()
    }

    /// A channel's store if it is open already; opens nothing.
    pub fn opened(&self, channel: Channel) -> Option<&Store<'static>> {
        self.slots[Stores::index(channel)].as_ref()
    }
}

/// The settings of one channel.
pub struct Store<'a> {
    store: Com,
    _control: Com,
    apo: &'a Apo,
    pub channel: Channel,
}

impl<'a> Store<'a> {
    pub fn len(&self) -> Result<i32> {
        unsafe {
            let count: unsafe extern "system" fn(*mut c_void, *mut i32) -> i32 = std::mem::transmute(self.store.entry(STORE_COUNT));
            let mut n = 0;
            check("IStore::GetSettingCount", count(self.store.0, &mut n))?;
            Ok(n)
        }
    }

    /// Setting by its identifier in the module's numbering.
    pub fn setting(&self, id: i32) -> Result<Setting<'a>> {
        self.fetch(STORE_OPEN, "IStore::OpenSetting", id)
    }

    /// Setting by position, for enumeration (`0..len()`).
    pub fn setting_at(&self, index: i32) -> Result<Setting<'a>> {
        self.fetch(STORE_AT, "IStore::GetSetting", index)
    }

    fn fetch(&self, entry: usize, what: &'static str, argument: i32) -> Result<Setting<'a>> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, i32, *mut *mut c_void) -> i32 = std::mem::transmute(self.store.entry(entry));
            let mut raw = std::ptr::null_mut();
            check(what, call(self.store.0, argument, &mut raw))?;
            if raw.is_null() {
                return Err(Error::Invalid(format!("{what}({argument}) returned nothing")));
            }
            Ok(Setting { ptr: Com(raw), apo: self.apo })
        }
    }

    /// Finds a setting by name, ignoring case and the `kSet_` prefix.
    pub fn find(&self, name: &str) -> Result<Setting<'a>> {
        let wanted = name.trim_start_matches("kSet_").to_ascii_lowercase();
        for index in 0..self.len()? {
            let setting = self.setting_at(index)?;
            if setting.name()?.trim_start_matches("kSet_").to_ascii_lowercase() == wanted {
                return Ok(setting);
            }
        }
        Err(Error::Invalid(format!("no setting named {name} on channel {}", self.channel.label())))
    }
}

/// One typed setting of a store.
pub struct Setting<'a> {
    ptr: Com,
    apo: &'a Apo,
}

impl Setting<'_> {
    unsafe fn out<T: Default>(&self, entry: usize, what: &'static str) -> Result<T> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut T) -> i32 = std::mem::transmute(self.ptr.entry(entry));
            let mut value = T::default();
            check(what, call(self.ptr.0, &mut value))?;
            Ok(value)
        }
    }

    unsafe fn text(&self, entry: usize, what: &'static str) -> Result<String> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut *mut u16) -> i32 = std::mem::transmute(self.ptr.entry(entry));
            let mut bstr = std::ptr::null_mut();
            check(what, call(self.ptr.0, &mut bstr))?;
            Ok(self.apo.strings.take(bstr))
        }
    }

    unsafe fn range<T: Default + Copy>(&self, entry: usize, what: &'static str) -> Result<(T, T, T)> {
        unsafe {
            let call: unsafe extern "system" fn(*mut c_void, *mut T, *mut T, *mut T) -> i32 = std::mem::transmute(self.ptr.entry(entry));
            let (mut default, mut min, mut max) = (T::default(), T::default(), T::default());
            check(what, call(self.ptr.0, &mut default, &mut min, &mut max))?;
            Ok((default, min, max))
        }
    }

    pub fn id(&self) -> Result<i32> {
        unsafe { self.out(SETTING_ID, "ISetting::GetSetting") }
    }

    pub fn name(&self) -> Result<String> {
        unsafe { self.text(SETTING_NAME, "ISetting::GetName") }
    }

    fn kind(&self) -> Result<i32> {
        unsafe { self.out(SETTING_KIND, "ISetting::GetType") }
    }

    pub fn value(&self) -> Result<Value> {
        unsafe {
            Ok(match self.kind()? {
                // Booleans travel as VARIANT_BOOL: 16 bits, any non-zero value is true.
                0 => Value::Bool(self.out::<i16>(SETTING_GET_BOOL, "ISetting::GetBoolSetting")? != 0),
                1 => Value::Int(self.out(SETTING_GET_LONG, "ISetting::GetLongSetting")?),
                2 => Value::Float(self.out(SETTING_GET_FLOAT, "ISetting::GetFloatSetting")?),
                5 => Value::Text(self.text(SETTING_GET_STRING, "ISetting::GetStringSetting")?),
                other => Value::Unsupported(other),
            })
        }
    }

    /// Allowed range as text, for numeric settings.
    pub fn range_text(&self) -> Result<Option<String>> {
        unsafe {
            Ok(match self.kind()? {
                1 => {
                    let (default, min, max) = self.range::<i32>(SETTING_LONG_TRAITS, "ISetting::GetLongTraits")?;
                    Some(format!("{min} .. {max}, default {default}"))
                }
                2 => {
                    let (default, min, max) = self.range::<f32>(SETTING_FLOAT_TRAITS, "ISetting::GetFloatTraits")?;
                    Some(format!("{min} .. {max}, default {default}"))
                }
                _ => None,
            })
        }
    }

    /// A numeric setting's value with its allowed range, as (value, minimum, maximum); `None` for
    /// settings that are not numbers.
    pub fn number(&self) -> Result<Option<(f32, f32, f32)>> {
        unsafe {
            Ok(match self.kind()? {
                1 => {
                    let (_, min, max) = self.range::<i32>(SETTING_LONG_TRAITS, "ISetting::GetLongTraits")?;
                    let value: i32 = self.out(SETTING_GET_LONG, "ISetting::GetLongSetting")?;
                    Some((value as f32, min as f32, max as f32))
                }
                2 => {
                    let (_, min, max) = self.range::<f32>(SETTING_FLOAT_TRAITS, "ISetting::GetFloatTraits")?;
                    let value: f32 = self.out(SETTING_GET_FLOAT, "ISetting::GetFloatSetting")?;
                    Some((value, min, max))
                }
                _ => None,
            })
        }
    }

    /// Writes a number into a numeric setting, rounded for whole-number settings and clamped to the
    /// allowed range.
    pub fn set_number(&self, value: f32) -> Result<()> {
        let Some((_, min, max)) = self.number()? else {
            return Err(Error::Invalid("this setting is not a number".into()));
        };
        let value = if value.is_finite() { value.clamp(min, max) } else { min };
        match self.kind()? {
            1 => self.set_from_text(&(value.round() as i32).to_string()),
            _ => self.set_from_text(&value.to_string()),
        }
    }

    /// Parses `text` according to the setting's type, checks it against the allowed range and writes it.
    /// The change applies to live audio immediately.
    pub fn set_from_text(&self, text: &str) -> Result<()> {
        let bad = |expected: &str| Error::Invalid(format!("\"{text}\" is not {expected}"));
        unsafe {
            match self.kind()? {
                0 => {
                    let value = match text.to_ascii_lowercase().as_str() {
                        "true" | "on" | "1" | "yes" => true,
                        "false" | "off" | "0" | "no" => false,
                        _ => return Err(bad("true or false")),
                    };
                    let call: unsafe extern "system" fn(*mut c_void, i16) -> i32 = std::mem::transmute(self.ptr.entry(SETTING_SET_BOOL));
                    check("ISetting::SetBoolSetting", call(self.ptr.0, if value { -1 } else { 0 }))
                }
                1 => {
                    let value: i32 = text.parse().map_err(|_| bad("a whole number"))?;
                    let (_, min, max) = self.range::<i32>(SETTING_LONG_TRAITS, "ISetting::GetLongTraits")?;
                    if value < min || value > max {
                        return Err(Error::Invalid(format!("{value} is outside {min} .. {max}")));
                    }
                    let call: unsafe extern "system" fn(*mut c_void, i32) -> i32 = std::mem::transmute(self.ptr.entry(SETTING_SET_LONG));
                    check("ISetting::SetLongSetting", call(self.ptr.0, value))
                }
                2 => {
                    let value: f32 = text.parse().map_err(|_| bad("a number"))?;
                    let (_, min, max) = self.range::<f32>(SETTING_FLOAT_TRAITS, "ISetting::GetFloatTraits")?;
                    if !value.is_finite() || value < min || value > max {
                        return Err(Error::Invalid(format!("{value} is outside {min} .. {max}")));
                    }
                    let call: unsafe extern "system" fn(*mut c_void, f32) -> i32 = std::mem::transmute(self.ptr.entry(SETTING_SET_FLOAT));
                    check("ISetting::SetFloatSetting", call(self.ptr.0, value))
                }
                5 => {
                    let call: unsafe extern "system" fn(*mut c_void, *mut u16) -> i32 = std::mem::transmute(self.ptr.entry(SETTING_SET_STRING));
                    check("ISetting::SetStringSetting", self.apo.strings.with(text, |bstr| call(self.ptr.0, bstr)))
                }
                other => Err(Error::Invalid(format!("settings of type {other} cannot be written yet"))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_channel_has_its_registry_key() {
        assert_eq!(Channel::Mic.registry_key(), r"SOFTWARE\SteelSeries ApS\Sonar.APO\ChatCapture\Settings\GlobalControl\Store");
        assert!(Channel::Chat.registry_key().contains(r"\ChatRender\"));
    }
}
