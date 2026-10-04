# OokCite

OokCite checks whether a citation is real, formats a bibliography in a CSL style, and stores references in a collection. It returns citation metadata only. It does not fetch PDFs or full text.

## Install from the marketplace

Add marketplace `TurtleTech-ehf/ookcite-mcp`, then install `ookcite@ookcite`. The plugin connects to `https://ookcite-api.turtletech.us/mcp`. Three skills load: format a pasted bibliography, verify references before submission, and build and export a collection. Sign in when asked. Formatting one citation and checking a short list of DOIs also work before sign-in, within the anonymous daily cap. Saving a collection requires the signed-in account.

## Install as a directory connector

On the web, desktop, and cowork apps, add the same HTTPS address as a connector and choose sign-in. Submit the connector by that URL, and submit this folder (`plugins/ookcite`) as the plugin bundle.

The plugin manifest sends the pre-registered public client id `ookcite-remote`, so the code client does not call a registration endpoint. Discovery for that client publishes S256 PKCE and no `registration_endpoint`. The protected-resource document lists that client's issuer first, and its `resource` field is `https://ookcite-api.turtletech.us/mcp`. The client accepts the directory callback exactly, and loopback redirects on `localhost` and `127.0.0.1` at any port.
