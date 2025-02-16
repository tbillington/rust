//! This is an incomplete implementation of mmap/munmap which is restricted in order to be
//! implementable on top of the existing memory system. The point of these function as-written is
//! to allow memory allocators written entirely in Rust to be executed by Miri. This implementation
//! does not support other uses of mmap such as file mappings.
//!
//! mmap/munmap behave a lot like alloc/dealloc, and for simple use they are exactly
//! equivalent. That is the only part we support: no MAP_FIXED or MAP_SHARED or anything
//! else that goes beyond a basic allocation API.
//!
//! Note that in addition to only supporting malloc-like calls to mmap, we only support free-like
//! calls to munmap, but for a very different reason. In principle, according to the man pages, it
//! is possible to unmap arbitrary regions of address space. But in a high-level language like Rust
//! this amounts to partial deallocation, which LLVM does not support. So any attempt to call our
//! munmap shim which would partily unmap a region of address space previously mapped by mmap will
//! report UB.

use crate::{helpers::round_to_next_multiple_of, *};
use rustc_target::abi::Size;

impl<'mir, 'tcx: 'mir> EvalContextExt<'mir, 'tcx> for crate::MiriInterpCx<'mir, 'tcx> {}
pub trait EvalContextExt<'mir, 'tcx: 'mir>: crate::MiriInterpCxExt<'mir, 'tcx> {
    fn mmap(
        &mut self,
        addr: &OpTy<'tcx, Provenance>,
        length: &OpTy<'tcx, Provenance>,
        prot: &OpTy<'tcx, Provenance>,
        flags: &OpTy<'tcx, Provenance>,
        fd: &OpTy<'tcx, Provenance>,
        offset: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        // We do not support MAP_FIXED, so the addr argument is always ignored (except for the MacOS hack)
        let addr = this.read_target_usize(addr)?;
        let length = this.read_target_usize(length)?;
        let prot = this.read_scalar(prot)?.to_i32()?;
        let flags = this.read_scalar(flags)?.to_i32()?;
        let fd = this.read_scalar(fd)?.to_i32()?;
        let offset = this.read_target_usize(offset)?;

        let map_private = this.eval_libc_i32("MAP_PRIVATE");
        let map_anonymous = this.eval_libc_i32("MAP_ANONYMOUS");
        let map_shared = this.eval_libc_i32("MAP_SHARED");
        let map_fixed = this.eval_libc_i32("MAP_FIXED");

        // This is a horrible hack, but on MacOS the guard page mechanism uses mmap
        // in a way we do not support. We just give it the return value it expects.
        if this.frame_in_std() && this.tcx.sess.target.os == "macos" && (flags & map_fixed) != 0 {
            return Ok(Scalar::from_maybe_pointer(Pointer::from_addr_invalid(addr), this));
        }

        let prot_read = this.eval_libc_i32("PROT_READ");
        let prot_write = this.eval_libc_i32("PROT_WRITE");

        // First, we do some basic argument validation as required by mmap
        if (flags & (map_private | map_shared)).count_ones() != 1 {
            this.set_last_error(Scalar::from_i32(this.eval_libc_i32("EINVAL")))?;
            return Ok(this.eval_libc("MAP_FAILED"));
        }
        if length == 0 {
            this.set_last_error(Scalar::from_i32(this.eval_libc_i32("EINVAL")))?;
            return Ok(this.eval_libc("MAP_FAILED"));
        }

        // If a user tries to map a file, we want to loudly inform them that this is not going
        // to work. It is possible that POSIX gives us enough leeway to return an error, but the
        // outcome for the user (I need to add cfg(miri)) is the same, just more frustrating.
        if fd != -1 {
            throw_unsup_format!("Miri does not support file-backed memory mappings");
        }

        // POSIX says:
        // [ENOTSUP]
        // * MAP_FIXED or MAP_PRIVATE was specified in the flags argument and the implementation
        // does not support this functionality.
        // * The implementation does not support the combination of accesses requested in the
        // prot argument.
        //
        // Miri doesn't support MAP_FIXED or any any protections other than PROT_READ|PROT_WRITE.
        if flags & map_fixed != 0 || prot != prot_read | prot_write {
            this.set_last_error(Scalar::from_i32(this.eval_libc_i32("ENOTSUP")))?;
            return Ok(this.eval_libc("MAP_FAILED"));
        }

        // Miri does not support shared mappings, or any of the other extensions that for example
        // Linux has added to the flags arguments.
        if flags != map_private | map_anonymous {
            throw_unsup_format!(
                "Miri only supports calls to mmap which set the flags argument to MAP_PRIVATE|MAP_ANONYMOUS"
            );
        }

        // This is only used for file mappings, which we don't support anyway.
        if offset != 0 {
            throw_unsup_format!("Miri does not support non-zero offsets to mmap");
        }

        let align = this.machine.page_align();
        let map_length = round_to_next_multiple_of(length, this.machine.page_size);

        let ptr =
            this.allocate_ptr(Size::from_bytes(map_length), align, MiriMemoryKind::Mmap.into())?;
        // We just allocated this, the access is definitely in-bounds and fits into our address space.
        // mmap guarantees new mappings are zero-init.
        this.write_bytes_ptr(
            ptr.into(),
            std::iter::repeat(0u8).take(usize::try_from(map_length).unwrap()),
        )
        .unwrap();

        Ok(Scalar::from_pointer(ptr, this))
    }

    fn munmap(
        &mut self,
        addr: &OpTy<'tcx, Provenance>,
        length: &OpTy<'tcx, Provenance>,
    ) -> InterpResult<'tcx, Scalar<Provenance>> {
        let this = self.eval_context_mut();

        let addr = this.read_pointer(addr)?;
        let length = this.read_target_usize(length)?;

        // addr must be a multiple of the page size, but apart from that munmap is just implemented
        // as a dealloc.
        #[allow(clippy::arithmetic_side_effects)] // PAGE_SIZE is nonzero
        if addr.addr().bytes() % this.machine.page_size != 0 {
            this.set_last_error(Scalar::from_i32(this.eval_libc_i32("EINVAL")))?;
            return Ok(Scalar::from_i32(-1));
        }

        let length = Size::from_bytes(round_to_next_multiple_of(length, this.machine.page_size));
        this.deallocate_ptr(
            addr,
            Some((length, this.machine.page_align())),
            MemoryKind::Machine(MiriMemoryKind::Mmap),
        )?;

        Ok(Scalar::from_i32(0))
    }
}
