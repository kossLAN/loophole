# Architecture

```
 Windows guest                                     Linux host
 ─────────────                                     ──────────
 application / DWM                                 loophole (crates/)
   │ Direct3D 10-12                                  ├─ loophole-vm: definitions, QEMU + swtpm, QMP
   ▼                                                 ├─ loophole-display: QEMU's D-Bus display
 loophole_umd.dll  (Triton DDI front end)            └─ window: Slint + GL, dma-buf textures
   ├─ DXVK (D3D11)  ─┐                                        ▲ dma-bufs, cursor     │ keys, pointer,
   └─ vkd3d-proton ──┤ Vulkan                                  │ (D-Bus, p2p)         │ size, refresh rate
      (D3D12)        ▼                                         │                      ▼
 vulkan_radeon.dll  (RADV, amdgpu winsys over vdrm)     QEMU: virtio-vga-gl, -display dbus
   │ D3DKMTEscape: submit, blobs, fences                  │ virtio-gpu native-context commands
   ▼                                                      ▼
 viogpu3d.sys  (WDDM display + render driver)  ═════▶ virglrenderer (amdgpu native context)
      virtqueues, hostmem window (blob BOs)               │ amdgpu ioctls
                                                          ▼
                                                     /dev/dri/renderD* ─▶ GPU
```

## GPU

RADV builds GFX command buffers, allocates buffer objects and assigns GPU virtual addresses
itself. Its amdgpu winsys talks through vdrm, the virtio-gpu native-context transport, which
loophole-guest's Mesa patches route over the kernel driver's `D3DKMTEscape` (ABI in its
`abi/viogpum.h`). virglrenderer replays those requests on the host GPU: it creates the buffer
objects, maps them into the guest through the host-memory window, submits and signals fences.
Commands are never translated.

## Direct3D

`loophole_umd.dll` is the WDDM user-mode driver for Direct3D 10-12, DWM included. Triton
(osy/virtio-win-mesa) implements the DDIs on DXVK and vkd3d-proton devices running in-process
on RADV (loophole-guest's `umd/`). Shared and presentable resources are guest blobs the kernel
driver can scan out, so DWM composes on the GPU. Direct3D 8/9 and OpenGL use DXVK and Zink placed next
to a game by `loophole layers enable`.

## Display

The kernel driver presents by pointing virtio-gpu's scanout at a blob. QEMU hands that buffer to
its D-Bus display client as a dma-buf (`ScanoutDMABUF2`), and QEMU holds the GPU's queue until the
client answers each update. loophole attaches through QMP
(`add_client` with a socket pair), imports each buffer once as an EGLImage in the window's GL
context and lets Slint draw the texture.

The window sends its size (`SetUIInfo`) and its monitor's refresh rate (a QEMU patch). QEMU puts
both in the virtual monitor's EDID and `loophole-agent.exe` switches the desktop to that mode.
The guest's pointer is a hardware cursor that the window draws at the host pointer.

Keys go to the guest as QEMU qnums, the pointer as tablet positions, and as raw relative motion
with the host pointer locked while a game keeps the pointer hidden. A QEMU patch switches the
active pointing device. The window reads buttons, motion and the wheel from winit, keeps every
button with whoever got the first press, and switches devices only while no guest button is held.

## VM

A definition (`crates/loophole-vm/src/config`) becomes one QEMU command line
(`crates/loophole-vm/src/argv.rs`): q35 with SMM, KVM, `-cpu host` with Hyper-V enlightenments,
memory in a memfd (optionally on huge pages), OVMF with Secure Boot support but no enrolled keys,
swtpm, `-blockdev` disks (images or host disks on NVMe, virtio-blk or SATA), e1000e or virtio-net
adapters, HD Audio to PipeWire, a USB tablet, USB and VFIO passthrough, and two CD drives whose
media change over QMP. The first NIC, disk, xHCI, GPU and HDA have fixed PCI
slots. `run::preflight` checks what the host must provide
before Start.

QEMU runs detached, so VMs outlive the window. A run's sockets, pid files and logs are in
`$XDG_RUNTIME_DIR/loophole/NAME`: `control.sock` for the window following the VM, `qmp.sock` for
short-lived clients.

## Builds

Nix builds the app and host side from one pinned nixpkgs, which the patch sets target. The app's
wrapper points it at the pinned QEMU, firmware and swtpm. The guest driver builds on GitHub Actions
in [kossLAN/loophole-guest](https://github.com/kossLAN/loophole-guest), and the app downloads the
latest release's disc unless `LOOPHOLE_DRIVER_DISC` names one.
