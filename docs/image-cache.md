# Image cache

## Storage and ownership

All remote artwork — in Dispatcharr TV, the channel logos the Guide draws — enters
`app/adapters/poster.rs` as a `tex::DIRECT` image: one fetched by its own URL, without a credential,
and fitted to the box it is drawn in. The adapter's demand workers read the disk tier before
fetching and decode off the render thread. `imgcache.rs` owns persistence; `ui::tex::TexCache`
owns GPU residency. Screens choose the image and size, without choosing a caching policy.

Disk entries contain compressed image bytes and a small versioned header with fetch time and
payload length. A direct image's SHA-256 filename (`imgcache::classify_direct`) names its own
versioned domain, the URL's origin, and the whole key — the URL and the box — so each size is its
own entry and two logos never share one. No request URL is stored as metadata. (The tier also keeps
the key shapes of the Plex image transcodes and baked collection art the PlxNative code used,
`classify` and `classify_baked`; nothing in Dispatcharr TV requests them. Until 2026-10-19 the
direct path asked `classify` for its key, which accepts only a Plex `/photo/:/transcode` request, so
no logo was ever written to disk.)

The cache chooses a writable persistent directory through `paths::image_cache_candidates`,
isolated by installation flavour (or simulator instance). It caps committed storage at **128 MiB**
and **8,192 files**, with **4 MiB** maximum compressed image size. One temporary write can consume
up to another image's size. A lazy startup scan builds a byte-accounted LRU index; normal reads
and writes use that index, avoiding repeated directory scans. Reads advance recency in memory;
file modification times persist recency at most once an hour per accessed entry. The header's
fetch timestamp never moves on a hit. Writes use a temporary file and rename, cache reads are
size-bounded, and corrupt entries are removed and fetched again. Unavailable storage is a cache
miss; a failed write cannot prevent the image from displaying.

On a disk hit older than **24 hours** a direct image is fetched afresh: the fetched logo replaces
the entry and is drawn, and only if that fetch fails is the stale copy drawn instead
(`poster::settle_direct`), so a logo changed on the server shows within a day and a server that is
down still shows the last one.

RAM residency remains independent of library size: 64 source slots and a 44 MiB GPU texture
budget. Demand workers stop claiming requests when combined decoded and pending-upload pixels
reach 8 MiB; already active decodes can temporarily exceed this admission threshold. If the GPU evicts an image while its source slot survives, the next draw requests its
pixels again through the same disk-first path, subject to the source's residency backoff (a card is
deferred only while its placement is still unknown, one sample; a moving card is no longer
deferred unless the dev trigger `dispatcharrtv-cardspeed=<px/s>` arms the decline). Requests
capture the cache epoch when queued, so work queued before an epoch change cannot read, remove,
write or publish images after it. Signing out of Dispatcharr does not clear the logos: they are the
server's public channel artwork, not a credential, and signing back in to the same server reuses
them.

## Stress testing

PlxNative stress-tested this tier on 2026-09-26 with `tools/image-cache-stress.py`, a synthetic Plex
library of 1,200 movies scrolled on its Library screen: every image survived a process restart
from disk. That screen is gone, so the procedure cannot be run as written. The tier's own host tests
(`rust-modules/platform/src/imgcache/tests.rs`) cover the limits, the LRU, recovery from corrupt
and orphaned files, and a thousand-image reopen; `poster.rs`'s tests cover the direct key and the
stale-refresh rule. A Guide with a few hundred channels is the natural device check.

## Frame-drop control run

A 60 FPS average does not rule out a noticeable stall. With `dispatcharrtv-framedrop` armed, the
heartbeat now measures consecutive Swap-to-Swap intervals as well as CPU work. Intentional idle
gaps are excluded. `frame_gt16`, `frame_gt33`, `frame_gt50` and `frame_gt100` count intervals
strictly longer than 1/60, 1/30, 1/20 and 1/10 second. The fixed-memory histogram reports rounded-up
millisecond percentile bounds per heartbeat, with an overflow percentile bounded by the observed
maximum. The report labels the worst per-window percentiles rather than presenting them as
percentiles of the entire run. `FRAMEDROP` lines separately explain slow CPU work by phase and
upload count. The existing `dropped=` field counts undeliverable UI messages and is **not** a
frame-drop counter.

For an A/B control on the same binary, arm `dispatcharrtv-imagecache-bypass` (a debug-build
trigger): it bypasses only the disk tier's reads and writes, while the bounded RAM/GPU tiers and the
frame budget stay the same.
