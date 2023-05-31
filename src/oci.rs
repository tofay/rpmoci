//! OCI image related functionality
//!
//! Copyright (C) Microsoft Corporation.
//!
//! This program is free software: you can redistribute it and/or modify
//! it under the terms of the GNU General Public License as published by
//! the Free Software Foundation, either version 3 of the License, or
//! (at your option) any later version.
//!
//! This program is distributed in the hope that it will be useful,
//! but WITHOUT ANY WARRANTY; without even the implied warranty of
//! MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//! GNU General Public License for more details.
//!
//! You should have received a copy of the GNU General Public License
//! along with this program.  If not, see <https://www.gnu.org/licenses/>.
use anyhow::{bail, Context, Result};
use flate2::{write::GzEncoder, Compression};
use oci_spec::image::{Descriptor, DescriptorBuilder, MediaType};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, os::unix::prelude::FileTypeExt, path::Path};
use tempfile::NamedTempFile;
use walkdir::WalkDir;

use crate::sha256_writer::Sha256Writer;

const OCI_LAYOUT_PATH: &str = "oci-layout";
// The only version we know
const OCI_LAYOUT_VERSION: &str = "1.0.0";

/// Initialize an [OCI image directory](https://github.com/opencontainers/image-spec/blob/main/image-layout.md) if required
///
/// If the directory doesn't exist, it will be created.
/// If the directory exists and is a valid OCI layout directory, return Ok.
/// Returns an error if the directory exists already and is not
/// an OCI image directory
pub(crate) fn init_image_directory(layout: impl AsRef<Path>) -> Result<(), anyhow::Error> {
    // If path exists, check whether it's a valid OCI image directory
    if layout.as_ref().exists() {
        match fs::read_dir(layout.as_ref()) {
            Ok(mut dir) => {
                // If this directory has an oci-layout file, check the version is one we know
                if let Some(oci_layout_entry) = dir.find(|entry| {
                    if let Ok(entry) = entry {
                        entry.file_name() == OCI_LAYOUT_PATH
                    } else {
                        false
                    }
                }) {
                    // read existing file, assert we can handle the version
                    let p = oci_layout_entry?.path();
                    let layout_file_contents = fs::read_to_string(&p)
                        .context(format!("Failed to read `{}`", p.display()))?;
                    let oci_layout: OciImageLayout = serde_json::from_str(&layout_file_contents)?;
                    // We only know 1.0.0 now
                    if oci_layout.version != OCI_LAYOUT_VERSION {
                        bail!(
                            "Unrecognized image layout version `{}` in {}",
                            oci_layout.version,
                            layout.as_ref().display()
                        )
                    }
                }
                // if the directory exists but is empty, then initialize it
                else if dir.count() == 0 {
                    init_dir(layout.as_ref())?;
                }
                // and if it's non-empty but doesn't have an oci-layout file, then bail
                else {
                    bail!(
                        "Directory exists but is not an OCI image directory: {}",
                        layout.as_ref().display()
                    )
                }
            }
            Err(e) => {
                return Err(e).context(format!("Failed to read `{}`", layout.as_ref().display()))
            }
        }
    } else {
        // Path doesn't exist so just create a new OCI image directory
        fs::create_dir_all(layout.as_ref()).context(format!(
            "Failed to create OCI image directory `{}`",
            layout.as_ref().display()
        ))?;

        init_dir(layout.as_ref())?;
    }
    Ok(())
}

/// An [OCI layout file](https://github.com/opencontainers/image-spec/blob/main/image-layout.md#oci-layout-file)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct OciImageLayout {
    #[serde(rename = "imageLayoutVersion")]
    version: String,
}

impl Default for OciImageLayout {
    fn default() -> Self {
        OciImageLayout {
            version: OCI_LAYOUT_VERSION.to_string(),
        }
    }
}

/// Create blobs/sha256, index.json and oci-layout file in a directory
fn init_dir(layout: impl AsRef<Path>) -> Result<(), anyhow::Error> {
    // Create blobs directory
    let blobs_dir = layout.as_ref().join("blobs").join("sha256");
    fs::create_dir_all(&blobs_dir).context(format!(
        "Failed to create blobs/sha256 directory `{}`",
        blobs_dir.display()
    ))?;

    // create oci-layout file
    let layout_path = layout.as_ref().join(OCI_LAYOUT_PATH);
    let layout_file = fs::File::create(&layout_path).context(format!(
        "Failed to create oci-layout file `{}`",
        layout_path.display()
    ))?;
    serde_json::to_writer(layout_file, &OciImageLayout::default()).context(format!(
        "Failed to write to oci-layout file `{}`",
        layout_path.display()
    ))?;

    // create image index
    let index = oci_spec::image::ImageIndexBuilder::default()
        .manifests(Vec::new())
        .schema_version(2u32)
        .build()?;
    let index_path = layout.as_ref().join("index.json");
    let index_file = std::fs::File::create(&index_path).context(format!(
        "Failed to create index.json file `{}`",
        index_path.display()
    ))?;
    serde_json::to_writer(index_file, &index).context(format!(
        "Failed to write to index.json file `{}`",
        index_path.display()
    ))?;

    Ok(())
}

/// Create a root filesystem image layer from a directory on disk.
/// The blob is written to the specified OCI layour directory.
///
/// Returns a Descriptor of the blob, and the [`diff_id`](https://github.com/opencontainers/image-spec/blob/main/config.md#layer-diffid) of the layer :
pub(crate) fn create_image_layer(
    rootfs_path: impl AsRef<Path>,
    layout_path: impl AsRef<Path>,
) -> Result<(Descriptor, String)> {
    // Remove sockets from the rootfs, otherwise tarring will fail.
    // Why? dnf and gpg seem to create sockets in cache.
    // tar-rs provides no way of ignoring these errors.
    // for comparison, umoci also fails when sockets are present but docker just ignores them
    for entry in WalkDir::new(rootfs_path.as_ref())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.metadata().map(|m| m.file_type().is_socket()).ok() == Some(true))
    {
        std::fs::remove_file(entry.path())?;
    }

    // We need to determine the sha256 hash of the compressed and uncompresssed blob.
    // The former for the blob id and the latter for the rootfs diff id which we need to include in the config blob.
    let enc = GzEncoder::new(
        Sha256Writer::new(NamedTempFile::new()?),
        Compression::fast(),
    );
    let mut tar = tar::Builder::new(Sha256Writer::new(enc));
    tar.follow_symlinks(false);
    tar.append_dir_all(".", rootfs_path.as_ref())
        .context("failed to archive root filesystem")?;
    let (diff_id_sha, gz) = tar.into_inner()?.finish();
    let (blob_digest, mut tmp_file) = gz.finish().context("failed to finish enc")?.finish();
    tmp_file.flush()?;

    let blob_path = layout_path.as_ref().join("blobs/sha256").join(&blob_digest);

    let (blob, tmp_path) = tmp_file.keep()?;
    let size: i64 = blob.metadata()?.len().try_into()?;
    // May fail if tempfile on different filesystem
    if fs::rename(&tmp_path, &blob_path).is_err() {
        fs::copy(&tmp_path, &blob_path).context(format!(
            "Failed to write image layer `{}`",
            blob_path.display()
        ))?;
    }

    Ok((
        DescriptorBuilder::default()
            .digest(format!("sha256:{}", blob_digest))
            .media_type(MediaType::ImageLayerGzip)
            .size(size)
            .build()?,
        format!("sha256:{}", diff_id_sha),
    ))
}

/// Write a json object with the specified media type to the specified
/// OCI layout directory
pub(crate) fn write_json_blob<T>(
    value: &T,
    media_type: MediaType,
    layout_path: impl AsRef<Path>,
) -> Result<Descriptor>
where
    T: ?Sized + Serialize,
{
    let mut writer = Sha256Writer::new(NamedTempFile::new()?);
    serde_json::to_writer(&mut writer, value)
        .context("Failed to write to blob to temporary file")?;
    writer.flush()?;
    let (blob_sha, tmp_file) = writer.finish();
    let blob_path = layout_path.as_ref().join("blobs/sha256").join(&blob_sha);

    let (blob, tmp_path) = tmp_file.keep()?;
    let size: i64 = blob.metadata()?.len().try_into()?;
    // May file if tempfile on different filesystem
    if fs::rename(&tmp_path, &blob_path).is_err() {
        fs::copy(&tmp_path, &blob_path)
            .context(format!("Failed to write blob `{}`", blob_path.display()))?;
    }

    Ok(DescriptorBuilder::default()
        .digest(format!("sha256:{}", blob_sha))
        .media_type(media_type)
        .size(size)
        .build()?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::init_image_directory;

    #[test]
    fn test_init() {
        let test_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/init/actual");
        let _ = std::fs::remove_dir_all(&test_dir);
        init_image_directory(&test_dir).unwrap();
    }
}
