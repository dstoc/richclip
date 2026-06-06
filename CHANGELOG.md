# Changelog

## [0.1.2](https://github.com/dstoc/richclip/compare/richclip-v0.1.1...richclip-v0.1.2) (2026-06-06)


### Features

* **contrib:** add image labeller ([064e45d](https://github.com/dstoc/richclip/commit/064e45d01a0ca90222c7347934059a49b0ce028c))
* **watch:** add image-aware filters ([7ebfedf](https://github.com/dstoc/richclip/commit/7ebfedf28f87c6b4b68f94e7a9ee5168d1993810))


### Bug Fixes

* **richclipd:** exit promptly on Ctrl-C ([427aa4a](https://github.com/dstoc/richclip/commit/427aa4a687cae02bcda026faa3432084ba29e79b))

## [0.1.1](https://github.com/dstoc/richclip/compare/richclip-v0.1.0...richclip-v0.1.1) (2026-06-05)


### Features

* **cli:** richclip CLI binary — add/list/formats/decode/update/delete/inspect ([b97d930](https://github.com/dstoc/richclip/commit/b97d930319a9dde1771738f1086c8ba6e5a43ed5))
* **contrib:** fuzzel picker script (picker proposal) ([e780dbe](https://github.com/dstoc/richclip/commit/e780dbe69791d87420f7809a38973608d1c7ef9c))
* **lib:** Phase 1 storage core — model, store, blobs, IPC/backend seams ([6079be3](https://github.com/dstoc/richclip/commit/6079be3c75c5eb89e32718847336794f1063f628))
* restore provider + richclip restore (Phase 2 complete) ([be02b2d](https://github.com/dstoc/richclip/commit/be02b2dc8fe954f4a257a40433d500f7d1437e09))
* richclipd daemon, IPC socket server, CLI watch + mutation routing ([c0b51fb](https://github.com/dstoc/richclip/commit/c0b51fb495fe1a2fdc18ce1f62a40069427e2fd7))
* **thumbnails:** foundation — image dep, cache paths, pure make_thumbnail ([9e75763](https://github.com/dstoc/richclip/commit/9e75763f604ef46dc39e4ff4c8a2b681ec63607d))
* **thumbnails:** generation hooks, CLI command, list field, cleanup ([d78c6fe](https://github.com/dstoc/richclip/commit/d78c6feeaf9d9d99d9da08ef89cac92a54d04c4f))
* **wayland:** wlr-data-control capture backend ([8f01733](https://github.com/dstoc/richclip/commit/8f0173330350214c0e6d1a95be0cd9705a9a65c9))


### Bug Fixes

* resolve clippy lints across the workspace ([bd7d9b8](https://github.com/dstoc/richclip/commit/bd7d9b860ad04922fa9f59e34a27234893568085))
* **wayland:** normalize captured MIME to base type (store text/plain, not text/plain;charset=utf-8) ([5dce311](https://github.com/dstoc/richclip/commit/5dce311e67566a75674a6235555e0513f415850a))
* **wayland:** restore panic — add event_created_child for device data_offer ([2132290](https://github.com/dstoc/richclip/commit/2132290dc6a57192aa6ae481cbba28b3be9ce10e))
