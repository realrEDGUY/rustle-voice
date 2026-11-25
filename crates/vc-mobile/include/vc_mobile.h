/*
 * vc_mobile.h: C bridge to the voicechat core (crates/vc-mobile), for the iOS app.
 * Written by hand; crates/vc-mobile/tests/golden.rs checks that every function the
 * library exports is declared here.
 *
 * Threads: every vc_app_* call is made on the main thread. vc_audio_capture and
 * vc_audio_render run on real-time audio threads and only use their VcAudio
 * handle. vc_pixels_release may be called from any thread.
 *
 * Strings returned as char * are freed with vc_string_free.
 */
#ifndef VC_MOBILE_H
#define VC_MOBILE_H

#include <stdbool.h>
#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct VcApp VcApp;
typedef struct VcAudio VcAudio;

/* Premultiplied RGBA8, row-major, len = width * height * 4 bytes. The pixels stay
 * valid until vc_pixels_release(handle). */
typedef struct {
    const uint8_t *data;
    size_t len;
    uint32_t width;
    uint32_t height;
    uint64_t generation;
    void *handle;
} VcPixels;

VcApp *vc_app_new(const char *profile_dir, const char *audio);   /* "external" | "fake-sine"; NULL on error */
void   vc_app_free(VcApp *app);
void   vc_app_step(VcApp *app);
char  *vc_app_command(VcApp *app, const char *json);  /* NULL = ok, else error text */
char  *vc_app_snapshot(VcApp *app);                   /* NULL = unchanged */
char  *vc_app_live(VcApp *app);                       /* NULL = unchanged */
bool   vc_app_poisoned(const VcApp *app);             /* a panic stopped the core */
void   vc_app_set_ptt(VcApp *app, bool down);
void   vc_app_set_audio_warning(VcApp *app, const char *msg_or_null);
bool   vc_app_image(VcApp *app, uint64_t key, uint64_t image_id, VcPixels *out);
bool   vc_app_screen(VcApp *app, uint64_t key, uint32_t user, VcPixels *out);
void   vc_pixels_release(void *handle);               /* any thread: drops the image */
VcAudio *vc_app_audio(VcApp *app);                    /* NULL unless audio=="external"; independent handle */
void   vc_audio_capture(const VcAudio *a, const float *mono, size_t n, uint32_t rate); /* real-time thread */
void   vc_audio_render(const VcAudio *a, float *mono, size_t n, uint32_t rate);        /* real-time thread */
void   vc_audio_free(VcAudio *a);
void   vc_string_free(char *s);
const char *vc_version(void);                         /* static, do not free */

#ifdef __cplusplus
}
#endif

#endif /* VC_MOBILE_H */
