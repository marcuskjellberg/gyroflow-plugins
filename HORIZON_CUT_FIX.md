# Horizon Leveling: zoom on one cut spilled into the other cuts

## What was wrong

When a clip is bladed on the timeline, Resolve copies the effect to every piece, including the
hidden `InstanceId` parameter. Horizon Leveling never gave a piece a new ID, and its manager
cache key only added the output aspect (`aspect:16:9`), not which part of the clip the piece
plays. So every cut of the clip got the **same** `StabilizationManager` from the shared cache.

Two pieces of state then went wrong:

1. **The crop.** Each instance only remembered *its own* last crop request. Cut A solved its
   zoom and wrote the fovs into the shared manager; cut B saw that its own request hadn't
   changed, skipped the solve, and rendered A's fovs. Change the zoom on one cut and the
   others showed it.
2. **Leveling and offset.** These go through the keyframe provider, which each instance
   re-points at its own values when it renders. The "did anything change" check only compared
   the instance's own previous values, so the manager could keep another cut's leveling/offset.

The same gap existed in Gyroflow Simple.

## What changed

- **Horizon's cache key now includes the cut's source range** (`range:{start}:{end}` from the
  timeline item, `range:none` until Resolve's scripting answers). Different cuts get different
  managers even with a copied ID.
- **The first user edit of a control splits the instance ID off** (Leveling, Zoom, Offset X/Y,
  Static crop, Crop smoothness in Horizon; Stabilization, Horizon level, Flip 180, Position X/Y,
  Zoom response in Simple), the same way the main Gyroflow plugin already does. It does not key
  on the OFX instance handle, which would split Resolve's own internal copies of one item and
  make them each re-solve.
- **Invalidation is per manager, not per instance.** The last solved crop request and the last
  applied leveling/offset (Simple: its settings) are recorded with the manager. If another
  instance solved or applied last with different values, the crop is solved again and the
  smoothing/zoom recomputed. Instances with identical values (Resolve's internal copies) still
  share one solve, and ranges within 3 frames merge rather than taking turns.
- Gyroflow Simple gets the ID split and the per-manager invalidation (it has no per-cut range;
  it solves over the whole clip, so unedited cuts sharing a manager is correct).
- Regression tests: two `CropState`s on one manager with different zoom (B must re-solve after
  A, and render its own zoom), two keyframe caches on one manager, the range context.
- `DEVELOPMENT.md` "Manager cache semantics" corrected.

## Install

Quit Resolve first (macOS kills a process whose loaded signed binary is overwritten), then:

```bash
cd ~/Developer/Gyroflow/gyroflow-plugins
just horizon dev   # already built: target/gyroflow-ofx-horizon-dev/HorizonLeveling.ofx.bundle
sudo rm -rf /Library/OFX/Plugins/HorizonLeveling.ofx.bundle && sudo cp -R target/gyroflow-ofx-horizon-dev/HorizonLeveling.ofx.bundle /Library/OFX/Plugins/
```

Start Resolve again.

## Verify

1. Put a clip on the timeline with Horizon Leveling, blade it into 2–3 pieces, play across them.
2. Check the log shows a distinct manager key per cut (the `range:` part differs):

   ```bash
   grep -a "new stab manager for key" ~/Library/Application\ Support/Gyroflow/gyroflow-openfx.log
   ```

   (The log is truncated on every Resolve launch.)
3. Change Zoom on one cut and confirm the other cuts don't change. Same for Leveling and Offset.

Note: cuts that already have a duplicated `InstanceId` from before this fix split off on their
first edit (the log says `Instance ID split off at the first edit`). Until then they're already
kept apart by the range in the key and by the per-manager invalidation.
