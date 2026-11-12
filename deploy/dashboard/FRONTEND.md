# Frontend source and build

The included Xiaomi card resource is built from
[`045c3ac946d4ef0a01201f306e9949643984a155`](https://github.com/PiotrMachowski/Lovelace-Xiaomi-Vacuum-Map-card/tree/045c3ac946d4ef0a01201f306e9949643984a155)
plus [xiaomi-room-source.patch](xiaomi-room-source.patch). Its local display
version is `narwal-room-source.3`; this is not an upstream release tag.
The original interface was previously exercised with upstream v2.4.1.

The patch adds one optional `room_source` entity to a map preset. That sensor
supplies current display-pixel room polygons, a map ID and catalog revision.
The card reads them as data, admits only bounded numeric polygons and safe
pins, and ignores incoming actions, variables and arbitrary label options.
The bridge marks labels actually painted into the image with
`label.rendered_in_image: true`. Other names, including unsupported punctuation,
Unicode and labels too wide for the raster font, use bounded plain text and a
finite, bounded position in escaped SVG text with zero offsets. This also works
when raster room labels are disabled. No label background is added.
Catalog changes clear selection. Ordinary HA updates continue through the
card, preserving the moving robot and current selection. Missing or invalid
catalog data clears selection and prevents dispatch.

`[[map_id_json]]` and `[[map_revision_json]]` are serialized current-catalog
variables used by the generated request payloads. The patch also applies
`max_selections` to manually drawn paths, allowing a two-point split line.
Its remaining build-only change limits the minifier to one worker.

The resource [resources/xiaomi-vacuum-map-card.js](resources/xiaomi-vacuum-map-card.js)
has SHA-256:

```text
2a4b045decd3e2fefbc99867de7b9fbffb37573a8b36b470f96017b99ca5a5b8
```

It is UTF-8 JavaScript with no source map, captured map, runtime credentials
or installation-specific entity mapping. Install the accompanying
[FRONTEND-LICENSES.txt](resources/FRONTEND-LICENSES.txt) with it. These notices
include the upstream MIT license and licenses of its runtime dependencies.

## Rebuild

Use Git, Node.js, npm and GNU `timeout` on Linux. The distributed resource was
built with the upstream lockfile, npm 10.9.3 and Node.js 24. The pinned tooling's
old tslib folder export needs a wildcard alias on modern Node; the build
script makes that isolated metadata adjustment without changing library code.
Package lifecycle scripts are disabled.

From the repository root, choose an output file that does not exist:

```sh
sh deploy/dashboard/build-frontend.sh /tmp/narwal-map-card-rebuilt.js
sha256sum /tmp/narwal-map-card-rebuilt.js
```

The script creates a temporary upstream checkout, verifies its revision,
applies the source patch, installs locked dependencies, builds and writes a
new output file. It removes its temporary checkout afterward. No HA or robot
connection is made. Build dependencies, complete upstream source, test files
and source maps are not included in this distribution.

## Save/Cancel template wrapper

Install Config Template Card through HACS; no copy is bundled here.
The inspected source is
[`17a4d22b76b011f45569901d98f0c5c5114d431d`](https://github.com/custom-cards/config-template-card/tree/17a4d22b76b011f45569901d98f0c5c5114d431d),
whose runtime version is **v1.3.6**. Its package manifest retains an older
version number. It is MIT licensed, copyright 2019 Ian Richardson.

This wrapper evaluates only the two Save/Cancel button configurations from
the editor sensor. Draft identifiers are read from HA state and passed to
`JSON.stringify`; room names are never inserted into executable expressions.
The live Xiaomi maps are not wrapped, because this wrapper does not forward
unwatched HA changes to a nested card.
