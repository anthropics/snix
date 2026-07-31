mod derivation {
    use crate::{
        derivation::{OutputHash, OutputName, Outputs, UnverifiedOutputsBuilder},
        store_path::StorePath,
        wire::{de::NixDeserialize, ser::NixSerialize},
    };
    use data_encoding::HEXLOWER;
    use nix_compat_derive::nix_serde_remote;

    nix_serde_remote!(
        #[nix(from_str, display)]
        OutputName
    );

    impl NixSerialize for Option<&'_ OutputHash> {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: crate::wire::ser::NixWrite,
        {
            if let Some(output_hash) = self.as_ref() {
                writer
                    .write_slice(output_hash.as_mode_and_algo_str().as_bytes())
                    .await?;
                writer
                    .write_display(
                        &data_encoding::HEXLOWER.encode_display(output_hash.hash.digest_as_bytes()),
                    )
                    .await?;
            } else {
                writer.write_slice(&[]).await?;
                writer.write_slice(&[]).await?;
            }
            Ok(())
        }
    }

    impl NixDeserialize for Option<OutputHash> {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + crate::wire::de::NixRead + Send,
        {
            use crate::wire::de::Error;
            if let Some(mode_and_algo) = reader.try_read_value::<String>().await? {
                let hash = reader.read_value::<String>().await?;
                if mode_and_algo.is_empty() && hash.is_empty() {
                    return Ok(Some(None));
                }
                let digest = HEXLOWER
                    .decode(hash.as_bytes())
                    .map_err(R::Error::invalid_data)?;
                Ok(Some(Some(
                    OutputHash::from_mode_algo_and_digest(&mode_and_algo, digest)
                        .map_err(R::Error::invalid_data)?,
                )))
            } else {
                Ok(None)
            }
        }
    }

    impl NixSerialize for Outputs {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: crate::wire::ser::NixWrite,
        {
            for (output_name, path) in self {
                writer.write_value(output_name).await?;
                writer.write_value(path).await?;
                writer.write_value(&self.as_fixed_output_hash()).await?;
            }
            Ok(())
        }
    }

    impl NixDeserialize for Outputs {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + crate::wire::de::NixRead + Send,
        {
            use crate::wire::de::Error;
            if let Some(len) = reader.try_read_value::<usize>().await? {
                let mut builder = UnverifiedOutputsBuilder::new();
                for _ in 0..len {
                    let output_name = reader.read_value::<OutputName>().await?;
                    let store_path = reader.read_value::<StorePath>().await?;
                    let output_hash = reader.read_value::<Option<OutputHash>>().await?;
                    builder
                        .try_insert(output_name, store_path, output_hash)
                        .map_err(R::Error::custom)?;
                }
                Ok(Some(builder.try_build().map_err(R::Error::custom)?))
            } else {
                Ok(None)
            }
        }
    }
}

mod derived_path {
    use crate::derived_path::{DerivedPath, LegacyDerivedPath};
    use nix_compat_derive::nix_serde_remote;

    nix_serde_remote!(
        #[nix(into = "LegacyDerivedPath", from = "LegacyDerivedPath")]
        DerivedPath
    );
    nix_serde_remote!(
        #[nix(from_str, display)]
        LegacyDerivedPath
    );
}

mod int {
    use nix_compat_derive::{nix_deserialize_remote, nix_serde_remote};

    nix_deserialize_remote!(
        #[nix(try_from = "u64")]
        u8
    );
    nix_serde_remote!(
        #[nix(try_from = "u64", into = "u64")]
        u16
    );
    nix_serde_remote!(
        #[nix(try_from = "u64", into = "u64")]
        u32
    );
    nix_serde_remote!(
        #[nix(try_from = "u64", try_into = "u64")]
        i64
    );
}

mod log {
    nix_compat_derive::nix_serde_remote!(
        #[nix(try_from = "u64", into = "u64")]
        crate::log::VerbosityLevel
    );
}

mod narinfo {
    use crate::narinfo::Signature;
    use crate::wire::de::{NixDeserialize, NixRead};

    nix_compat_derive::nix_serialize_remote!(#[nix(display)] Signature<String>);

    impl NixDeserialize for Signature<String> {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + NixRead + Send,
        {
            use crate::wire::de::Error;
            let value: Option<String> = reader.try_read_value().await?;
            match value {
                Some(value) => Ok(Some(
                    Signature::<String>::parse(&value).map_err(R::Error::invalid_data)?,
                )),
                None => Ok(None),
            }
        }
    }
}

mod nixhash {
    use nix_compat_derive::nix_serde_remote;

    use crate::nixhash::{CAHash, HashAlgo};
    use crate::wire::de::{NixDeserialize, NixRead};
    use crate::wire::ser::{NixSerialize, NixWrite};

    nix_serde_remote!(
        #[nix(display, from_str)]
        HashAlgo
    );

    impl NixSerialize for CAHash {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: NixWrite,
        {
            writer.write_value(&self.to_string()).await
        }
    }

    impl NixDeserialize for CAHash {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + NixRead + Send,
        {
            use crate::wire::de::Error;
            let value: Option<String> = reader.try_read_value().await?;
            match value {
                Some(value) => Ok(Some(CAHash::from_nix_hex_str(&value).ok_or_else(|| {
                    R::Error::invalid_data(format!("Invalid cahash {value}"))
                })?)),
                None => Ok(None),
            }
        }
    }

    impl NixSerialize for Option<CAHash> {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: NixWrite,
        {
            match self {
                Some(value) => writer.write_value(value).await,
                None => writer.write_value("").await,
            }
        }
    }

    impl NixDeserialize for Option<CAHash> {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + NixRead + Send,
        {
            use crate::wire::de::Error;
            let value: Option<String> = reader.try_read_value().await?;
            match value {
                Some(value) => {
                    if value.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(Some(CAHash::from_nix_hex_str(&value).ok_or_else(
                            || R::Error::invalid_data(format!("Invalid cahash {value}")),
                        )?)))
                    }
                }
                None => Ok(None),
            }
        }
    }
}

mod realisation {
    use nix_compat_derive::nix_serde_remote;

    use crate::realisation::{DrvOutput, Realisation};
    use crate::wire::de::NixDeserialize;
    use crate::wire::ser::NixSerialize;

    nix_serde_remote!(
        #[nix(from_str, display)]
        DrvOutput
    );

    impl NixSerialize for Realisation {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: crate::wire::ser::NixWrite,
        {
            use crate::wire::ser::Error;
            let s = serde_json::to_string(&self).map_err(W::Error::custom)?;
            writer.write_slice(s.as_bytes()).await
        }
    }

    impl NixDeserialize for Realisation {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + crate::wire::de::NixRead + Send,
        {
            use crate::wire::de::Error;
            if let Some(buf) = reader.try_read_bytes().await? {
                Ok(Some(
                    serde_json::from_slice(&buf).map_err(R::Error::custom)?,
                ))
            } else {
                Ok(None)
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use rstest::rstest;

        use crate::{btree_map, hash_set, realisation::Realisation};

        #[tokio::test]
        #[rstest]
        #[case(
            Realisation {
                id: "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad!out".parse().unwrap(),
                out_path: "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3".parse().unwrap(),
                signatures: hash_set!["cache.nixos.org-1:0CpHca+06TwFp9VkMyz5OaphT3E8mnS+1SWymYlvFaghKSYPCMQ66TS1XPAr1+y9rfQZPLaHrBjjnIRktE/nAA=="],
                dependent_realisations: btree_map![
                    "sha256:ba7816bf8f01cfea414140de5dae2223b00361a496177a9cf410ff61f20015ad!dev" => "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-dev",
                    "sha256:ba7816bf8f01cfea414140de5dae2223b00361a696177a9cf410ff61f20015ad!bin" => "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-bin",

                ],
            },
            "{\"id\":\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad!out\",\"outPath\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3\",\"signatures\":[\"cache.nixos.org-1:0CpHca+06TwFp9VkMyz5OaphT3E8mnS+1SWymYlvFaghKSYPCMQ66TS1XPAr1+y9rfQZPLaHrBjjnIRktE/nAA==\"],\"dependentRealisations\":{\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a496177a9cf410ff61f20015ad!dev\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-dev\",\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a696177a9cf410ff61f20015ad!bin\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-bin\"}}",
        )]
        async fn nix_write_realisation(#[case] value: Realisation, #[case] expected: &str) {
            use crate::wire::ser::NixWrite as _;

            let mut mock = crate::test::wire::ser::Builder::new()
                .write_slice(expected.as_bytes())
                .build();
            mock.write_value(&value).await.unwrap();
        }

        #[tokio::test]
        #[rstest]
        #[case(
            Realisation {
                id: "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad!out".parse().unwrap(),
                out_path: "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3".parse().unwrap(),
                signatures: hash_set!["cache.nixos.org-1:0CpHca+06TwFp9VkMyz5OaphT3E8mnS+1SWymYlvFaghKSYPCMQ66TS1XPAr1+y9rfQZPLaHrBjjnIRktE/nAA=="],
                dependent_realisations: btree_map![
                    "sha256:ba7816bf8f01cfea414140de5dae2223b00361a496177a9cf410ff61f20015ad!dev" => "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-dev",
                    "sha256:ba7816bf8f01cfea414140de5dae2223b00361a696177a9cf410ff61f20015ad!bin" => "7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-bin",

                ],
            },
            "{\"id\":\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad!out\",\"outPath\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3\",\"signatures\":[\"cache.nixos.org-1:0CpHca+06TwFp9VkMyz5OaphT3E8mnS+1SWymYlvFaghKSYPCMQ66TS1XPAr1+y9rfQZPLaHrBjjnIRktE/nAA==\"],\"dependentRealisations\":{\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a496177a9cf410ff61f20015ad!dev\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-dev\",\"sha256:ba7816bf8f01cfea414140de5dae2223b00361a696177a9cf410ff61f20015ad!bin\":\"7h7qgvs4kgzsn8a6rb273saxyqh4jxlz-konsole-18.12.3-bin\"}}",
        )]
        async fn nix_read_realisation(#[case] expected: Realisation, #[case] value: &str) {
            use crate::wire::de::NixRead as _;

            let mut mock = crate::test::wire::de::Builder::new()
                .read_slice(value.as_bytes())
                .build();
            let actual: Realisation = mock.read_value().await.unwrap();
            pretty_assertions::assert_eq!(actual, expected);
        }
    }
}

mod store_path {
    use crate::store_path::StorePath;
    use crate::wire::de::{NixDeserialize, NixRead};
    use crate::wire::ser::{NixSerialize, NixWrite};

    // Custom implementation since FromStr does not use from_absolute_path
    impl NixDeserialize for StorePath {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + NixRead + Send,
        {
            use crate::wire::de::Error;
            if let Some(buf) = reader.try_read_bytes().await? {
                let result = StorePath::from_absolute_path(&buf);
                result.map(Some).map_err(R::Error::invalid_data)
            } else {
                Ok(None)
            }
        }
    }

    // Custom implementation since Display does not use absolute paths.
    impl NixSerialize for StorePath {
        fn serialize<W>(&self, writer: &mut W) -> impl Future<Output = Result<(), W::Error>> + Send
        where
            W: NixWrite,
        {
            let sp = self.as_absolute_path_fmt();
            async move { writer.write_display(&sp).await }
        }
    }

    impl NixDeserialize for Option<StorePath> {
        async fn try_deserialize<R>(reader: &mut R) -> Result<Option<Self>, R::Error>
        where
            R: ?Sized + NixRead + Send,
        {
            use crate::wire::de::Error;
            if let Some(buf) = reader.try_read_bytes().await? {
                if buf.is_empty() {
                    Ok(Some(None))
                } else {
                    let result = StorePath::from_absolute_path(&buf);
                    result
                        .map(|r| Some(Some(r)))
                        .map_err(R::Error::invalid_data)
                }
            } else {
                Ok(Some(None))
            }
        }
    }

    // Writes StorePath or an empty string.
    impl NixSerialize for Option<StorePath> {
        async fn serialize<W>(&self, writer: &mut W) -> Result<(), W::Error>
        where
            W: NixWrite,
        {
            match self {
                Some(value) => writer.write_value(value).await,
                None => writer.write_value("").await,
            }
        }
    }
}
