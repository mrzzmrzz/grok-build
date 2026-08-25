# Apple code signing and notarization

The macOS release job signs `grok` with a Developer ID Application certificate
and submits the signed binary to Apple's notary service before uploading the
artifact. Signing and notarization credentials are removed before the job
finishes. Never commit them to the repository.

## Required repository secrets

Configure these Actions secrets under **Settings → Secrets and variables →
Actions**:

| Secret | Value |
| --- | --- |
| `APPLE_CERTIFICATE_BASE64` | Base64-encoded `.p12` containing the Developer ID Application certificate and private key |
| `APPLE_CERTIFICATE_PASSWORD` | Password used when exporting the `.p12` |
| `APPLE_SIGNING_IDENTITY` | Full identity: `Developer ID Application: YY T (6F8LHK4H5P)` |
| `APPLE_NOTARY_KEY` | Contents of the App Store Connect Team API key `.p8` file |
| `APPLE_NOTARY_KEY_ID` | App Store Connect Team API key ID |
| `APPLE_NOTARY_ISSUER_ID` | App Store Connect API issuer ID |

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
- Create the notary API key under **App Store Connect → Users and Access →
  Integrations → Team Keys** with Developer access. It must belong to the same
  developer team as the signing certificate.

The workflow applies a secure timestamp and the hardened runtime, then runs
`codesign --verify`. It submits a temporary ZIP containing the signed binary,
waits for an `Accepted` result, prints the notarization log, and verifies the
published binary with `spctl`. Standalone executables cannot carry a stapled
ticket, so Gatekeeper retrieves their notarization ticket from Apple.
