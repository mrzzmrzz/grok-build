# Apple code signing

The macOS release job signs `grok` with a Developer ID Application certificate
before uploading the artifact. The certificate is loaded into an ephemeral
keychain and removed before the job finishes. Never commit the certificate or
its password to the repository.

## Required repository secrets

Configure these Actions secrets under **Settings → Secrets and variables →
Actions**:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Base64-encoded `.p12` containing the Developer ID Application certificate and private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity: `Developer ID Application: YY T (6F8LHK4H5P)` |

Encode the certificate on macOS without adding line breaks:

```sh
base64 -i DeveloperIDApplication.p12 | tr -d '\n' | pbcopy
```

Paste the clipboard contents into `APPLE_CERTIFICATE_BASE64`. The workflow
fails the macOS build with a configuration error if any required secret is
missing; Linux builds never receive these secrets.

## Certificate requirements

- Export the certificate and private key together as a password-protected
  PKCS#12 (`.p12`) file.
- Use a **Developer ID Application** certificate for software distributed
  outside the Mac App Store.
- Set `APPLE_SIGNING_IDENTITY` exactly as shown by
  `security find-identity -v -p codesigning` after importing the certificate.

The workflow applies a secure timestamp and the hardened runtime, then runs
`codesign --verify` before publishing the binary. Code signing does not by
itself notarize the artifact; notarization requires separate App Store Connect
credentials and a submission step.
