//! Phase 3 — Gestion Mémoire Unifiée (`xmem` ↔ Vulkan `VK_EXT_external_memory_host`).
//!
//! Alloue de la mémoire alignée sur page via `mmap` et importe directement le pointeur CPU
//! dans Vulkan (`VkDeviceMemory` + `VkBuffer`) afin que les ressources Xbox allouées par CPU
//! soient consommables zéro-copie par le GPU.

use std::collections::BTreeMap;
use std::ptr;
use std::sync::Arc;

use ash::vk;
use parking_lot::RwLock;
use sb_common::{sb_debug, sb_warn};

use crate::vulkan::GpuContext;

#[derive(Debug, Clone, Copy)]
pub struct UnifiedRegion {
    pub cpu_addr: usize,
    pub requested_size: usize,
    pub mapped_size: usize,
    pub alignment: usize,
    pub vk_buffer: Option<vk::Buffer>,
    pub vk_memory: Option<vk::DeviceMemory>,
}

impl UnifiedRegion {
    #[inline]
    pub const fn end_addr(&self) -> usize {
        self.cpu_addr.saturating_add(self.mapped_size)
    }

    #[inline]
    pub const fn contains(&self, addr: usize) -> bool {
        addr >= self.cpu_addr && addr < self.end_addr()
    }
}

pub struct UnifiedMemoryManager {
    gpu: Option<Arc<GpuContext>>,
    regions: RwLock<BTreeMap<usize, UnifiedRegion>>,
}

impl UnifiedMemoryManager {
    pub fn new(gpu: Option<Arc<GpuContext>>) -> Self {
        Self {
            gpu,
            regions: RwLock::new(BTreeMap::new()),
        }
    }

    /// Alloue une plage mémoire unifiée alignée et l'importe dans Vulkan via `VK_EXT_external_memory_host`.
    pub fn alloc(&self, size: usize, alignment: usize, zero_init: bool) -> Option<*mut u8> {
        let min_host_align = self
            .gpu
            .as_ref()
            .map(|g| g.caps.min_imported_host_pointer_alignment as usize)
            .unwrap_or(4096);
        let eff_align = alignment.max(4096).max(min_host_align).next_power_of_two();
        let mapped_size = align_up(size.max(4096), eff_align)?;

        // `mmap` anonyme Linux retourne une mémoire déjà mise à zéro et alignée sur page (4 KiB).
        // Pour les alignements > 4 KiB (64 KiB, 2 MiB), sur-alloue puis ajuste la base alignée.
        let (raw_ptr, alloc_len) = if eff_align <= 4096 {
            // SAFETY: `mmap` anonyme RW.
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    mapped_size,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                return None;
            }
            (p.cast::<u8>(), mapped_size)
        } else {
            let over = mapped_size.checked_add(eff_align)?;
            // SAFETY: `mmap` anonyme RW avec marge d'alignement.
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    over,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                return None;
            }
            let base = p as usize;
            let aligned = align_up(base, eff_align)?;
            let prefix = aligned - base;
            let suffix = over - prefix - mapped_size;
            if prefix > 0 {
                // SAFETY: libère le préfixe aligné page inutilisé.
                unsafe {
                    libc::munmap(base as *mut libc::c_void, prefix);
                }
            }
            if suffix > 0 {
                // SAFETY: libère le suffixe aligné page inutilisé.
                unsafe {
                    libc::munmap((aligned + mapped_size) as *mut libc::c_void, suffix);
                }
            }
            (aligned as *mut u8, mapped_size)
        };

        let _ = zero_init; // `MAP_ANONYMOUS` garantit déjà des pages nulles.

        let (vk_buffer, vk_memory) = self.try_import_into_vulkan(raw_ptr, alloc_len);

        let region = UnifiedRegion {
            cpu_addr: raw_ptr as usize,
            requested_size: size,
            mapped_size: alloc_len,
            alignment: eff_align,
            vk_buffer,
            vk_memory,
        };
        self.regions.write().insert(region.cpu_addr, region);
        Some(raw_ptr)
    }

    fn try_import_into_vulkan(
        &self,
        host_ptr: *mut u8,
        size: usize,
    ) -> (Option<vk::Buffer>, Option<vk::DeviceMemory>) {
        let Some(ref gpu) = self.gpu else {
            return (None, None);
        };
        let Some(ref ext_fn) = gpu.ext_memory_host_fn else {
            return (None, None);
        };

        let mut ext_buf_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT);
        let buf_info = vk::BufferCreateInfo::default()
            .size(size as u64)
            .usage(
                vk::BufferUsageFlags::TRANSFER_SRC
                    | vk::BufferUsageFlags::TRANSFER_DST
                    | vk::BufferUsageFlags::VERTEX_BUFFER
                    | vk::BufferUsageFlags::INDEX_BUFFER
                    | vk::BufferUsageFlags::UNIFORM_BUFFER
                    | vk::BufferUsageFlags::STORAGE_BUFFER,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .push_next(&mut ext_buf_info);

        // SAFETY: création d'un `VkBuffer` importable depuis un pointeur hôte.
        let Ok(buffer) = (unsafe { gpu.device.create_buffer(&buf_info, None) }) else {
            return (None, None);
        };

        let mut host_props = vk::MemoryHostPointerPropertiesEXT::default();
        // SAFETY: interroge les `memoryTypeBits` compatibles pour `host_ptr`.
        let prop_res = unsafe {
            (ext_fn.fp().get_memory_host_pointer_properties_ext)(
                gpu.device.handle(),
                vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT,
                host_ptr.cast(),
                &mut host_props,
            )
        };
        if prop_res != vk::Result::SUCCESS {
            sb_warn!("xmem", "get_memory_host_pointer_properties: {prop_res:?}");
            unsafe { gpu.device.destroy_buffer(buffer, None) };
            return (None, None);
        }

        // SAFETY: obtient les exigences mémoire du buffer.
        let mem_reqs = unsafe { gpu.device.get_buffer_memory_requirements(buffer) };
        let combined_bits = mem_reqs.memory_type_bits & host_props.memory_type_bits;
        let Some(mem_type_idx) =
            gpu.find_memory_type(combined_bits, vk::MemoryPropertyFlags::HOST_VISIBLE)
        else {
            unsafe { gpu.device.destroy_buffer(buffer, None) };
            return (None, None);
        };

        let mut import_info = vk::ImportMemoryHostPointerInfoEXT::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::HOST_ALLOCATION_EXT)
            .host_pointer(host_ptr.cast());
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(size as u64)
            .memory_type_index(mem_type_idx)
            .push_next(&mut import_info);

        // SAFETY: importe `host_ptr` en zéro-copie dans `VkDeviceMemory`.
        let memory = match unsafe { gpu.device.allocate_memory(&alloc_info, None) } {
            Ok(mem) => mem,
            Err(err) => {
                sb_warn!(
                    "xmem",
                    "vkAllocateMemory(HOST_ALLOCATION_EXT) a échoué : {err}"
                );
                unsafe { gpu.device.destroy_buffer(buffer, None) };
                return (None, None);
            }
        };

        // SAFETY: lie le buffer au `VkDeviceMemory` importé.
        if let Err(err) = unsafe { gpu.device.bind_buffer_memory(buffer, memory, 0) } {
            sb_warn!("xmem", "vkBindBufferMemory a échoué : {err}");
            unsafe {
                gpu.device.free_memory(memory, None);
                gpu.device.destroy_buffer(buffer, None);
            }
            return (None, None);
        }

        sb_debug!(
            "xmem",
            "mémoire unifiée zéro-copie importée dans Vulkan : ptr={host_ptr:p}, size={size:#x}"
        );
        (Some(buffer), Some(memory))
    }

    /// Libère la région mémoire unifiée (et détruit le `VkBuffer` / `VkDeviceMemory` associé).
    ///
    /// # Safety
    /// Plus aucun accès CPU ou GPU ne doit référencer `ptr`.
    pub unsafe fn free(&self, ptr_val: *mut u8) -> bool {
        if ptr_val.is_null() {
            return true;
        }
        let Some(region) = self.regions.write().remove(&(ptr_val as usize)) else {
            return false;
        };

        if let Some(ref gpu) = self.gpu {
            // SAFETY: destruction Vulkan avant `munmap`.
            unsafe {
                if let Some(buf) = region.vk_buffer {
                    gpu.device.destroy_buffer(buf, None);
                }
                if let Some(mem) = region.vk_memory {
                    gpu.device.free_memory(mem, None);
                }
            }
        }

        // SAFETY: `region.cpu_addr..+region.mapped_size` provient de notre `mmap`.
        unsafe {
            libc::munmap(region.cpu_addr as *mut libc::c_void, region.mapped_size);
        }
        true
    }

    /// Résout une adresse CPU quelconque vers sa région unifiée (et son offset GPU).
    pub fn find_region(&self, addr: usize) -> Option<(UnifiedRegion, usize)> {
        let map = self.regions.read();
        let (_, &region) = map.range(..=addr).next_back()?;
        if region.contains(addr) {
            Some((region, addr - region.cpu_addr))
        } else {
            None
        }
    }

    pub fn size_of(&self, ptr_val: *const u8) -> usize {
        self.regions
            .read()
            .get(&(ptr_val as usize))
            .map(|r| r.requested_size)
            .unwrap_or(0)
    }
}

fn align_up(value: usize, align: usize) -> Option<usize> {
    let rem = value % align;
    if rem == 0 {
        Some(value)
    } else {
        value.checked_add(align - rem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates_64k_aligned_unified_memory_and_imports_into_vulkan() {
        let gpu = GpuContext::new(false).ok();
        let has_host_ext = gpu
            .as_ref()
            .is_some_and(|g| g.caps.has_external_memory_host);
        let mgr = UnifiedMemoryManager::new(gpu);

        let ptr = mgr
            .alloc(65536, 65536, true)
            .expect("allocation unifiée réussie");
        assert!((ptr as usize).is_multiple_of(65536));

        let (region, offset) = mgr
            .find_region((ptr as usize) + 512)
            .expect("région retrouvée");
        assert_eq!(offset, 512);
        if has_host_ext {
            assert!(
                region.vk_buffer.is_some() && region.vk_memory.is_some(),
                "VK_EXT_external_memory_host doit importer la mémoire CPU en zéro-copie"
            );
        }

        // SAFETY: aucune référence active.
        assert!(unsafe { mgr.free(ptr) });
        assert!(mgr.find_region(ptr as usize).is_none());
    }
}
