# OokCite

OokCite checks whether a citation is real, formats a bibliography in a CSL style, and stores references in a collection. It returns citation metadata only. It does not fetch PDFs or full text.

## Install from the marketplace

Add marketplace `TurtleTech-ehf/ookcite-mcp`, then install `ookcite@ookcite`. The plugin connects to `https://ookcite-api.turtletech.us/mcp`. Three skills load: format a pasted bibliography, verify references before submission, and build and export a collection. Sign in when asked. Formatting one citation and checking a short list of DOIs also work before sign-in, within the anonymous daily cap. Saving a collection requires the signed-in account.

## Install as a directory connector

On the web, desktop, and cowork apps, add the same HTTPS address as a connector and choose sign-in. Submit the connector by that URL, and submit this folder (`plugins/ookcite`) as the plugin bundle.

The authorization server named by the protected-resource metadata must publish either a `registration_endpoint` or client-id metadata document support (`client_id_metadata_document_supported` true, and `none` among `token_endpoint_auth_methods_supported`). It must accept the directory's hosted callback and port-agnostic loopback redirects on `localhost` and `127.0.0.1`. The protected-resource document's `resource` field must be the MCP URL, including `/mcp`.
