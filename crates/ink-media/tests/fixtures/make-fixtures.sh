#!/usr/bin/env bash
# Regenerates the MP3 test files in this directory.
#
# Requires ffmpeg with libmp3lame. The files are not needed to run the
# tests: they are committed. Run this only to change them.
set -euo pipefail
cd "$(dirname "$0")"

DURATION=0.5

# A cosine wave starts at its peak, so a file that starts with silence (the
# delay of an MP3 encoder) can be told from one that does not. The left and
# right channels have different frequencies (440 Hz and 660 Hz), and the
# amplitude is 0.5.
stereo() { # <sample rate>
  echo "aevalsrc=0.5*cos(2*PI*440*t)|0.5*cos(2*PI*660*t):s=$1:d=$DURATION"
}
mono() { # <sample rate>
  echo "aevalsrc=0.5*cos(2*PI*440*t):s=$1:d=$DURATION"
}
COMMON=(-loglevel error -y -fflags +bitexact -flags:a +bitexact -c:a libmp3lame -b:a 128k)

# 48 kHz stereo with the encoder's Xing / LAME header, which has the delay and
# the padding (gapless playback).
ffmpeg -f lavfi -i "$(stereo 48000)" "${COMMON[@]}" tone-48k-stereo-gapless.mp3

# 44.1 kHz stereo with an ID3v2 tag in front.
ffmpeg -f lavfi -i "$(stereo 44100)" "${COMMON[@]}" \
  -id3v2_version 3 -metadata title="test tone" \
  -metadata comment="generated sine wave, no third-party content" \
  tone-44k-stereo-id3.mp3

# 44.1 kHz mono.
ffmpeg -f lavfi -i "$(mono 44100)" "${COMMON[@]}" tone-44k-mono.mp3

# 48 kHz stereo without the Xing / LAME header: the decoder cannot know the
# delay, so the file starts with a short silence (a known limit, ADR-0009).
ffmpeg -f lavfi -i "$(stereo 48000)" "${COMMON[@]}" -write_xing 0 tone-48k-stereo-no-header.mp3
