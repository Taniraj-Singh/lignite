# ClusterFuzzLite build image for lignite (Rust / cargo-fuzz).
FROM gcr.io/oss-fuzz-base/base-builder-rust

# Bring the whole repository into the image at the build root ($SRC).
COPY . $SRC/
WORKDIR $SRC

# The build script that compiles the fuzz targets into $OUT.
COPY .clusterfuzzlite/build.sh $SRC/
