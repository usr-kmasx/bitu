//! Camada Vulkan do Bitu: intercepta vkQueuePresentKHR para o pacing.
//! Encadeamento conforme a documentação oficial do Vulkan-Loader (VK_LAYER_LINK_INFO).
#![allow(non_camel_case_types)]

use crate::pacing;
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::{c_char, c_void};
use std::sync::{LazyLock, RwLock};

pub type VkInstance = *mut c_void;
pub type VkDevice = *mut c_void;
pub type VkPhysicalDevice = *mut c_void;
pub type VkQueue = *mut c_void;
pub type VkResult = i32;
pub type PFN_vkVoidFunction = Option<unsafe extern "system" fn()>;

type PfnGipa = unsafe extern "system" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction;
type PfnGdpa = unsafe extern "system" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction;
type PfnCreateInstance =
    unsafe extern "system" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult;
type PfnCreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const c_void,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
type PfnEnumeratePhysDevices =
    unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type PfnDestroyInstance = unsafe extern "system" fn(VkInstance, *const c_void);
type PfnDestroyDevice = unsafe extern "system" fn(VkDevice, *const c_void);
type PfnGetDeviceQueue = unsafe extern "system" fn(VkDevice, u32, u32, *mut VkQueue) ;
type PfnGetDeviceQueue2 =
    unsafe extern "system" fn(VkDevice, *const c_void, *mut VkQueue);
type PfnPresent = unsafe extern "system" fn(VkQueue, *const c_void) -> VkResult;

const VK_LOADER_INSTANCE_CREATE_INFO: i32 = 47;
const VK_LOADER_DEVICE_CREATE_INFO: i32 = 48;
const VK_LAYER_LINK_INFO: i32 = 0;

const VK_SUCCESS: i32 = 0;
const VK_INCOMPLETE: i32 = 5;
const VK_ERROR_INITIALIZATION_FAILED: i32 = -6;
const VK_ERROR_LAYER_NOT_PRESENT: i32 = -6;

const N_CREATE_INSTANCE: &CStr = c"vkCreateInstance";
const N_GIPA: &CStr = c"vkGetInstanceProcAddr";
const N_GDPA: &CStr = c"vkGetDeviceProcAddr";
const N_CREATE_DEVICE: &CStr = c"vkCreateDevice";
const N_DESTROY_INSTANCE: &CStr = c"vkDestroyInstance";
const N_ENUM_PHYS: &CStr = c"vkEnumeratePhysicalDevices";
const N_PRESENT: &CStr = c"vkQueuePresentKHR";
const N_DESTROY_DEVICE: &CStr = c"vkDestroyDevice";
const N_GET_QUEUE: &CStr = c"vkGetDeviceQueue";
const N_GET_QUEUE2: &CStr = c"vkGetDeviceQueue2";

#[inline]
unsafe fn name_is(p: *const c_char, s: &CStr) -> bool {
    CStr::from_ptr(p) == s
}

#[inline]
fn cname(s: &CStr) -> *const c_char {
    s.as_ptr()
}

/// Converte um PFN_vkVoidFunction (Option<fn()>) para um tipo concreto de função.
#[inline]
unsafe fn from_void<F: Copy>(v: PFN_vkVoidFunction) -> Option<F> {
    match v {
        None => None,
        Some(f) => Some(
            std::mem::transmute_copy::<unsafe extern "system" fn(), F>(&f)
        ),
    }
}

/// Converte qualquer fn ptr concreto para o PFN "void" do loader.
#[inline]
fn to_void<F>(f: F) -> PFN_vkVoidFunction {
    unsafe { std::mem::transmute_copy::<F, PFN_vkVoidFunction>(&f) }
}

// ------------------------------------------------------------- structs C

#[repr(C)]
struct LinkInfoHdr {
    s_type: i32,
    p_next: *const c_void,
    function: i32,
    /// início da union: *mut InstanceLink ou *mut DeviceLink
    u_p_layer_info: *mut c_void,
}

#[repr(C)]
struct InstanceLink {
    p_next: *mut InstanceLink,
    next_gipa: PfnGipa,
    #[allow(dead_code)]
    next_pgpda: usize,
}

#[repr(C)]
struct DeviceLink {
    p_next: *mut DeviceLink,
    next_gipa: PfnGipa,
    next_gdpa: PfnGdpa,
}

/// Procura na cadeia pNext o struct do loader (sType/lugar correspondente).
unsafe fn find_chain_link(doc: *const c_void, s_type: i32) -> Option<*mut LinkInfoHdr> {
    let mut cur = doc as *const LinkInfoHdr;
    while !cur.is_null() {
        if (*cur).s_type == s_type && (*cur).function == VK_LAYER_LINK_INFO {
            return Some(cur as *mut LinkInfoHdr);
        }
        cur = (*cur).p_next as *const LinkInfoHdr;
    }
    None
}

/// Propriedades de layer (VkLayerProperties).
#[repr(C)]
pub struct VkLayerProperties {
    layer_name: [u8; 256],
    spec_version: u32,
    implementation_version: u32,
    description: [u8; 256],
}
unsafe impl Send for VkLayerProperties {}
unsafe impl Sync for VkLayerProperties {}

// --------------------------------------------------------------- estado

struct InstanceData {
    next_gipa: PfnGipa,
}
unsafe impl Send for InstanceData {}
unsafe impl Sync for InstanceData {}

struct DeviceData {
    next_gdpa: PfnGdpa,
    next_present: Option<PfnPresent>,
    next_destroy: Option<PfnDestroyDevice>,
}
unsafe impl Send for DeviceData {}
unsafe impl Sync for DeviceData {}

static INSTANCES: LazyLock<RwLock<HashMap<usize, InstanceData>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
/// physical device -> instance
static PHYSICALS: LazyLock<RwLock<HashMap<usize, usize>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
static DEVICES: LazyLock<RwLock<HashMap<usize, DeviceData>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
/// queue -> device
static QUEUES: LazyLock<RwLock<HashMap<usize, usize>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));
fn pacing_debug() -> bool {
    std::env::var("BITU_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

// -------------------------------------------------------- vkCreateInstance

#[no_mangle]
pub unsafe extern "system" fn vkCreateInstance(
    p_create_info: *const c_void,
    p_allocator: *const c_void,
    p_instance: *mut VkInstance,
) -> VkResult {
    let Some(ci) = find_chain_link(p_create_info, VK_LOADER_INSTANCE_CREATE_INFO) else {
        if pacing_debug() {
            eprintln!("[bitu-layer] chain info não encontrado");
        }
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let link = (*ci).u_p_layer_info as *mut InstanceLink;
    if link.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).next_gipa;
    let Some(next_create) =
        from_void::<PfnCreateInstance>(next_gipa(std::ptr::null_mut(), cname(N_CREATE_INSTANCE)))
    else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    // avança o link para a próxima camada da cadeia
    (*ci).u_p_layer_info = (*link).p_next as *mut c_void;

    let res = next_create(p_create_info, p_allocator, p_instance);
    if res != VK_SUCCESS {
        return res;
    }

    if let Ok(mut m) = INSTANCES.write() {
        m.insert((*p_instance) as usize, InstanceData { next_gipa });
    }
    if pacing_debug() {
        eprintln!("[bitu-layer] vkCreateInstance ok");
    }
    res
}

// ------------------------------------------------- wrappers de instância

unsafe extern "system" fn my_enumerate_phys_devices(
    instance: VkInstance,
    p_count: *mut u32,
    p_phys: *mut VkPhysicalDevice,
) -> VkResult {
    let enumer = {
        let m = match INSTANCES.read() {
            Ok(m) => m,
            Err(_) => return VK_INCOMPLETE,
        };
        let Some(d) = m.get(&(instance as usize)) else {
            return VK_INCOMPLETE;
        };
        from_void::<PfnEnumeratePhysDevices>((d.next_gipa)(instance, cname(N_ENUM_PHYS)))
    };
    let Some(enumer) = enumer else {
        return VK_INCOMPLETE;
    };
    let res = enumer(instance, p_count, p_phys);
    if res == VK_SUCCESS && !p_count.is_null() && !p_phys.is_null() {
        let count = *p_count;
        if count > 0 {
            if let Ok(mut m) = PHYSICALS.write() {
                for i in 0..count {
                    m.insert(*p_phys.add(i as usize) as usize, instance as usize);
                }
            }
        }
    }
    res
}

unsafe extern "system" fn my_destroy_instance(instance: VkInstance, p_allocator: *const c_void) {
    let destroy = {
        let m = match INSTANCES.read() {
            Ok(m) => m,
            Err(_) => return,
        };
        let Some(d) = m.get(&(instance as usize)) else { return };
        from_void::<PfnDestroyInstance>((d.next_gipa)(instance, cname(N_DESTROY_INSTANCE)))
    };
    if let Some(f) = destroy {
        f(instance, p_allocator);
    }
    let phys_keys: Vec<usize> = match PHYSICALS.read() {
        Ok(m) => m
            .iter()
            .filter(|(_, &i)| i == instance as usize)
            .map(|(&k, _)| k)
            .collect(),
        Err(_) => Vec::new(),
    };
    if let Ok(mut m) = PHYSICALS.write() {
        for k in phys_keys {
            m.remove(&k);
        }
    }
    if let Ok(mut m) = INSTANCES.write() {
        m.remove(&(instance as usize));
    }
}

unsafe extern "system" fn my_create_device(
    phys: VkPhysicalDevice,
    p_create_info: *const c_void,
    p_allocator: *const c_void,
    p_device: *mut VkDevice,
) -> VkResult {
    let Some(ci) = find_chain_link(p_create_info, VK_LOADER_DEVICE_CREATE_INFO) else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let link = (*ci).u_p_layer_info as *mut DeviceLink;
    if link.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let next_gipa = (*link).next_gipa;
    let next_gdpa = (*link).next_gdpa;

    let instance = match PHYSICALS.read() {
        Ok(m) => m.get(&(phys as usize)).copied().unwrap_or(0),
        Err(_) => 0,
    } as VkInstance;

    let Some(next_create) =
        from_void::<PfnCreateDevice>(next_gipa(instance, cname(N_CREATE_DEVICE)))
    else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    // avança o link para a próxima camada
    (*ci).u_p_layer_info = (*link).p_next as *mut c_void;

    let res = next_create(phys, p_create_info, p_allocator, p_device);
    if res != VK_SUCCESS {
        return res;
    }

    let device = *p_device;
    let next_present = from_void::<PfnPresent>((next_gdpa)(device, cname(N_PRESENT)));
    let next_destroy = from_void::<PfnDestroyDevice>((next_gdpa)(device, cname(N_DESTROY_DEVICE)));
    if let Ok(mut m) = DEVICES.write() {
        m.insert(
            device as usize,
            DeviceData {
                next_gdpa,
                next_present,
                next_destroy,
            },
        );
    }
    if pacing_debug() {
        eprintln!("[bitu-layer] vkCreateDevice ok, hook de present instalado");
    }
    res
}

// --------------------------------------------------- vkGet*ProcAddr

#[no_mangle]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: VkInstance,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    if name_is(p_name, N_CREATE_INSTANCE) {
        let f: PfnCreateInstance = vkCreateInstance;
        return to_void(f);
    }
    if name_is(p_name, N_GIPA) {
            let f: PfnGipa = vkGetInstanceProcAddr;
        return to_void(f);
    }
    if name_is(p_name, N_GDPA) {
            let f: PfnGdpa = vkGetDeviceProcAddr;
        return to_void(f);
    }
    if name_is(p_name, N_CREATE_DEVICE) {
            let f: PfnCreateDevice = my_create_device;
        return to_void(f);
    }
    if name_is(p_name, N_DESTROY_INSTANCE) {
            let f: PfnDestroyInstance = my_destroy_instance;
        return to_void(f);
    }
    if name_is(p_name, N_ENUM_PHYS) {
            let f: PfnEnumeratePhysDevices = my_enumerate_phys_devices;
        return to_void(f);
    }

    // demais funções: cadeia para baixo (abaixo de nós)
    let next_gipa = match INSTANCES.read() {
        Ok(m) => m.get(&(instance as usize)).map(|d| d.next_gipa),
        Err(_) => None,
    };
    if let Some(g) = next_gipa {
        return g(instance, p_name);
    }
    None
}

#[no_mangle]
pub unsafe extern "system" fn vkGetDeviceProcAddr(
    device: VkDevice,
    p_name: *const c_char,
) -> PFN_vkVoidFunction {
    if p_name.is_null() {
        return None;
    }
    if name_is(p_name, N_PRESENT) {
            let f: PfnPresent = my_queue_present;
        return to_void(f);
    }
    if name_is(p_name, N_DESTROY_DEVICE) {
            let f: PfnDestroyDevice = my_destroy_device;
        return to_void(f);
    }
    if name_is(p_name, N_GDPA) {
            let f: PfnGdpa = vkGetDeviceProcAddr;
        return to_void(f);
    }
    if name_is(p_name, N_GET_QUEUE) {
            let f: PfnGetDeviceQueue = my_get_device_queue;
        return to_void(f);
    }
    if name_is(p_name, N_GET_QUEUE2) {
            let f: PfnGetDeviceQueue2 = my_get_device_queue2;
        return to_void(f);
    }

    // demais: cadeia para baixo
    let next_gdpa = read_device_chain(device, |d| d.next_gdpa);
    if let Some(g) = next_gdpa {
        return g(device, p_name);
    }
    None
}

/// helper: lê o DeviceData de um device e extrai um campo fn.
fn read_device_chain<F, T>(device: VkDevice, get: F) -> Option<T>
where
    F: FnOnce(&DeviceData) -> T,
{
    match DEVICES.read() {
        Ok(m) => m.get(&(device as usize)).map(get),
        Err(_) => None,
    }
}

// --------------------------------------------------------- hooks de device

unsafe extern "system" fn my_queue_present(
    queue: VkQueue,
    p_present_info: *const c_void,
) -> VkResult {
    pacing::frame();
    let next_present: Option<PfnPresent> = {
        let queue_addr = queue as usize;
        let m = match QUEUES.read() {
            Ok(m) => m,
            Err(_) => return VK_SUCCESS,
        };
        let device = match m.get(&queue_addr) {
            Some(&d) => d,
            None => {
                // fallback: um único device no processo
                let dm = match DEVICES.read() {
                    Ok(dm) => dm,
                    Err(_) => return VK_SUCCESS,
                };
                if dm.len() != 1 {
                    return VK_SUCCESS;
                }
                *dm.keys().next().unwrap()
            }
        };
        let dm = match DEVICES.read() {
            Ok(dm) => dm,
            Err(_) => return VK_SUCCESS,
        };
        dm.get(&device).and_then(|d| d.next_present)
    };
    match next_present {
        Some(f) => f(queue, p_present_info),
        None => VK_SUCCESS,
    }
}

unsafe extern "system" fn my_destroy_device(device: VkDevice, p_allocator: *const c_void) {
    let destroy = read_device_chain(device, |d| d.next_destroy);
    if let Some(Some(f)) = destroy {
        f(device, p_allocator);
    }
    let qkeys: Vec<usize> = match QUEUES.read() {
        Ok(m) => m
            .iter()
            .filter(|(_, &d)| d == device as usize)
            .map(|(&k, _)| k)
            .collect(),
        Err(_) => Vec::new(),
    };
    if let Ok(mut m) = QUEUES.write() {
        for k in qkeys {
            m.remove(&k);
        }
    }
    if let Ok(mut m) = DEVICES.write() {
        m.remove(&(device as usize));
    }
}

unsafe extern "system" fn my_get_device_queue(
    device: VkDevice,
    family: u32,
    index: u32,
    p_queue: *mut VkQueue,
) {
    let next = read_device_chain(device, |d| d.next_gdpa);
    let Some(g) = next else { return };
    let Some(f) = from_void::<PfnGetDeviceQueue>(g(device, cname(N_GET_QUEUE))) else {
        return;
    };
    f(device, family, index, p_queue);
    if !p_queue.is_null() {
        if let Ok(mut m) = QUEUES.write() {
            m.insert((*p_queue) as usize, device as usize);
        }
    }
}

unsafe extern "system" fn my_get_device_queue2(
    device: VkDevice,
    p_info: *const c_void,
    p_queue: *mut VkQueue,
) {
    let next = read_device_chain(device, |d| d.next_gdpa);
    let Some(g) = next else { return };
    let Some(f) = from_void::<PfnGetDeviceQueue2>(g(device, cname(N_GET_QUEUE2))) else {
        return;
    };
    f(device, p_info, p_queue);
    if !p_queue.is_null() {
        if let Ok(mut m) = QUEUES.write() {
            m.insert((*p_queue) as usize, device as usize);
        }
    }
}

// --------------------------------------------------- funções globais de enum

#[no_mangle]
pub unsafe extern "system" fn vkEnumerateInstanceLayerProperties(
    p_count: *mut u32,
    p_props: *mut VkLayerProperties,
) -> VkResult {
    let count = 1u32;
    if p_count.is_null() {
        return VK_INCOMPLETE;
    }
    if p_props.is_null() {
        *p_count = count;
        return VK_SUCCESS;
    }
    if *p_count < count {
        *p_count = count;
        return VK_INCOMPLETE;
    }
    let mut lp = VkLayerProperties {
        layer_name: [0; 256],
        spec_version: (1u32 << 22) | (3u32 << 12), // 1.3.0 variante 0
        implementation_version: 1,
        description: [0; 256],
    };
    lp.layer_name[..15].copy_from_slice(b"VK_LAYER_BITU_1");
    lp.description[..24].copy_from_slice(b"Bitu frame pacing layer\0");
    *p_props = lp;
    *p_count = count;
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "system" fn vkEnumerateInstanceExtensionProperties(
    p_layer_name: *const c_char,
    p_count: *mut u32,
    p_props: *mut c_void,
) -> VkResult {
    #[allow(unused_mut)]
    let _ = p_props;
    if p_layer_name.is_null() || CStr::from_ptr(p_layer_name).to_bytes() == b"VK_LAYER_BITU_1" {
        if !p_count.is_null() {
            *p_count = 0;
        }
        return VK_SUCCESS;
    }
    VK_ERROR_LAYER_NOT_PRESENT
}

#[repr(C)]
pub struct NegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    pfn_get_instance_proc_addr: usize,
    pfn_get_device_proc_addr: usize,
    pfn_get_physical_device_proc_addr: usize,
}

#[no_mangle]

pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    p_version: *mut NegotiateLayerInterface,
) -> VkResult {

    // LAYER_NEGOTIATE_INTERFACE_STRUCT = 1
    if (*p_version).s_type == 1 {
        // suportamos apenas a versão 1 (sem vk_layerGetPhysicalDeviceProcAddr)
        (*p_version).loader_layer_interface_version = 1;
        (*p_version).pfn_get_instance_proc_addr = vkGetInstanceProcAddr as *const () as usize;
        (*p_version).pfn_get_device_proc_addr = vkGetDeviceProcAddr as *const () as usize;
        return VK_SUCCESS;
    }
    VK_ERROR_INITIALIZATION_FAILED
}
