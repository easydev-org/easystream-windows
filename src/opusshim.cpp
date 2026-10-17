// Tiny libopus wrapper: PCM (s16le, interleaved) in, Opus packets out. Keeps
// the Rust side free of libopus's structs. One encoder per stream.
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <opus/opus.h>

extern "C" {

typedef struct {
    OpusEncoder *enc;
    int rate, channels, frame_samples; // frame_samples = samples PER CHANNEL
} na_opus;

// na_opus_open(rate, channels, bitrate) — 48 kHz, 20 ms frames by convention.
void *na_opus_open(int rate, int channels, int bitrate) {
    int err = 0;
    na_opus *o = (na_opus *)calloc(1, sizeof(na_opus));
    if (!o) return NULL;
    o->enc = opus_encoder_create(rate, channels, OPUS_APPLICATION_AUDIO, &err);
    if (!o->enc) { free(o); return NULL; }
    opus_encoder_ctl(o->enc, OPUS_SET_BITRATE(bitrate));
    // Music-ish content (a desktop's audio), not just speech.
    opus_encoder_ctl(o->enc, OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));
    o->rate = rate;
    o->channels = channels;
    o->frame_samples = rate / 50; // 20 ms
    return o;
}

// na_opus_frame_samples returns the per-channel sample count of one 20 ms frame.
int na_opus_frame_samples(void *h) { return ((na_opus *)h)->frame_samples; }

// na_opus_encode encodes ONE 20 ms frame (frame_samples * channels s16 samples)
// into `out` (must be >= 4000 bytes). Returns the packet length, or <0 on error.
int na_opus_encode(void *h, const int16_t *pcm, unsigned char *out, int out_cap) {
    na_opus *o = (na_opus *)h;
    int n = opus_encode(o->enc, pcm, o->frame_samples, out, out_cap);
    return n;
}

void na_opus_close(void *h) {
    na_opus *o = (na_opus *)h;
    if (!o) return;
    if (o->enc) opus_encoder_destroy(o->enc);
    free(o);
}

} // extern "C"
