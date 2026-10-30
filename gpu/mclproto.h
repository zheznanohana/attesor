// Wire format of the Attesor Metal bridge fast path (shared by mclmetal.m and mclbridge.m).
//
// A batch (op 4) is:   MclMsgHdr, then records. Every record starts with a 32-byte header and is a
// multiple of 8 bytes long; arguments follow the header. Little endian on both sides.
//
//   word0  len (u32) | kind (u8) << 32 | nargs (u8) << 40 | flags (u16) << 48
//   word1  selector id (u32) | aux (u32) << 32
//   word2  target handle
//   word3  result handle (MCL_R_CREATE: handle chosen by the guest)
//
// Argument: 8-byte header (tag | len << 32), then `len` payload bytes padded to 8.
// Op 6 returns a borrowed MclCaps (never FREE); op 7 is a VT plist RPC (FREE its reply).
// Selector ids are defined with op 5 (name) before their first use; they are process wide.
#ifndef MCLPROTO_H
#define MCLPROTO_H
#include <stdint.h>
#include <stddef.h>
#include <string.h>

#define MCL_MAGIC 0x424c434du   // 'MCLB'

enum { MCL_OP_RPC = 1, MCL_OP_FREE = 2, MCL_OP_EVENT = 3, MCL_OP_BATCH = 4, MCL_OP_DEFSEL = 5, MCL_OP_CAPS = 6, MCL_OP_VT = 7 };

// pad was reserved/zero in v1. New peers negotiate before using v2 arguments.
#define MCL_VERSION 2u
#define MCL_CAP_SHARED_UPLOAD 1u
#define MCL_CAP_VT 2u
typedef struct { uint32_t version, capabilities; } MclCaps;
typedef struct { uint32_t magic, cmdbytes, flags, version; } MclMsgHdr;
typedef struct { uint64_t address, length; } MclSharedData;


enum {
    MCL_R_MSG = 1,     // fire and forget (void result)
    MCL_R_CREATE = 2,  // fire and forget, result object gets the guest-chosen handle in word3
    MCL_R_SYNC = 3,    // last record of the message; the host replies with MclReply
    MCL_R_DROP = 4,    // release one reference of `target`
};

enum {
    MCL_A_NIL = 1,     // nil object / NULL pointer
    MCL_A_HANDLE = 2,  // 8 bytes: object handle
    MCL_A_SCALAR = 3,  // raw bytes of a scalar / struct argument
    MCL_A_IDARRAY = 4, // 8 bytes per element: handles (0 = nil)
    MCL_A_DATA = 5,    // raw bytes; the argument is a pointer to a copy
    MCL_A_PLIST = 6,   // binary plist of an encoded object graph (descriptors, arrays, ...)
    MCL_A_STRING = 7,  // utf8 NSString
    MCL_A_DESC = 8,    // binary descriptor (see desc_emit in mclmetal.m)
    MCL_A_SHARED = 10, // MclSharedData: immutable snapshot in the shared address space; valid until batch acknowledgement
    MCL_A_BLOCK = 9,   // completion handler: u64 block id, u8 n, n type chars of the block arguments
};

enum {
    MCL_T_VOID = 0,
    MCL_T_SCALAR = 1,  // value: raw bits
    MCL_T_NIL = 2,
    MCL_T_HANDLE = 3,  // value: handle, cls: class id; blob: plist {c,p} the first time the class is mentioned
    MCL_T_PLIST = 4,   // blob: plist
    MCL_T_EXC = 5,     // blob: utf8 message
};

typedef struct { uint32_t tag, cls; uint64_t value; uint32_t blen, pad; } MclReply;   // blob follows

#define MCL_GUEST_HANDLE_BASE (1ull << 40)

// Validate the entire envelope before executing any records (including legacy v1 batches).
// Shared addresses are trusted in-process pointers, not a cross-process security boundary.
static inline int mcl_batch_valid(const void *bytes, size_t length) {
    if (!bytes || length < sizeof(MclMsgHdr)) return 0;
    MclMsgHdr h; memcpy(&h, bytes, sizeof h);
    if (h.magic != MCL_MAGIC || (h.version && h.version != MCL_VERSION) || h.flags ||
        h.cmdbytes != length - sizeof h) return 0;
    const uint8_t *p = (const uint8_t *)bytes + sizeof h;
    size_t left = h.cmdbytes;
    while (left) {
        if (left < 32) return 0;
        uint64_t w; memcpy(&w, p, 8);
        uint32_t len = (uint32_t)w; uint8_t kind = w >> 32, nargs = w >> 40;
        if (len < 32 || len > left || len % 8 || w >> 48 || kind < MCL_R_MSG || kind > MCL_R_DROP) return 0;
        if ((kind == MCL_R_SYNC && len != left) || (kind == MCL_R_DROP && (nargs || len != 32))) return 0;
        size_t off = 32;
        for (unsigned i = 0; i < nargs; i++) {
            if (len - off < 8) return 0;
            uint64_t arg; memcpy(&arg, p + off, 8);
            uint32_t tag = (uint32_t)arg, n = arg >> 32;
            size_t padded = ((size_t)n + 7) & ~(size_t)7;
            if (tag < MCL_A_NIL || tag > MCL_A_SHARED || padded > len - off - 8) return 0;
            if ((tag == MCL_A_NIL && n) || (tag == MCL_A_HANDLE && n != 8) ||
                (tag == MCL_A_SHARED && (!h.version || n != sizeof(MclSharedData))) ||
                (tag == MCL_A_IDARRAY && n % 8) || (tag == MCL_A_BLOCK && n < 9)) return 0;
            if (tag == MCL_A_BLOCK && (p[off + 16] > 15 || n != 9u + p[off + 16])) return 0;
            off += 8 + padded;
        }
        if (off != len) return 0;
        p += len; left -= len;
    }
    return 1;
}
#endif
