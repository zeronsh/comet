# Source-control brand SVG assets

Researched 2026-08-22 against first-party sources. These are trademark assets, not open-source code dependencies; preserve the supplied artwork and keep the provider relationship explicit in the UI.

## Recommended assets

| Provider | Official SVG source | Desktop-app fit | Licensing / use conclusion |
| --- | --- | --- | --- |
| GitHub | [`GitHub_Logos.zip`](https://brand.github.com/GitHub_Logos.zip), specifically `GitHub Logos/SVG/GitHub_Invertocat_Black.svg` or `GitHub_Invertocat_White.svg` | Good for a small provider/integration indicator. The official bundle also contains black/white lockups and clear-space variants. | GitHub does not grant a blanket copyright or trademark license for the bundle. Its policy expressly lists showing that a project integrates with GitHub as a permitted use, but prohibits implying affiliation, using the mark as the app's own logo, or modifying it. Use the unmodified official SVG and obtain written approval if the use is outside that integration context. See the [GitHub logo guidelines](https://brand.github.com/foundations/logo) and [GitHub application terms](https://docs.github.com/en/site-policy/github-terms/github-open-source-applications-terms-and-conditions). |
| Bitbucket | [`Bitbucket-icon-blue.svg`](https://wac-cdn.atlassian.com/dam/jcr%3A6bb63fa2-de51-41f3-aaa5-3e324ee9c74b/Bitbucket-icon-blue.svg?cdnVersion=3627), linked from Atlassian's [trademark page](https://www.atlassian.com/legal/trademark) | Good as a standalone provider icon; the SVG has a compact `62.4 × 56.13` viewBox and no wordmark. Atlassian also exposes the official product logo library through its [press kit](https://www.atlassian.com/company/news/press-kit). | Atlassian treats Bitbucket as a trademark, not as a freely licensed artwork. Its public guidance permits identifying Atlassian products when the use is unmodified, non-deceptive, and does not create confusion; product logos may also identify compatibility. Re-size only, do not recolor/rebuild/combine it with the app logo, and do not imply Atlassian endorsement. Other uses require Atlassian approval. |

## Practical decision

Use the official black/white GitHub Invertocat and the official blue Bitbucket icon as provider indicators, with accessible labels such as “GitHub” and “Bitbucket.” Do not treat either SVG as the app's logo or as an MIT/Apache-style dependency. Keep the source URL and a copy of the applicable guidelines with release notices; re-check the guidelines before a public release because both providers reserve the right to update them.

## Verification notes

- The GitHub download was checked to contain SVG files, including the named Invertocat variants; the bundle did not present a separate permissive software license.
- The Bitbucket URL is an official `wac-cdn.atlassian.com` SVG linked by Atlassian's own trademark guidance, not a third-party icon catalog.
- This note is a usage summary, not legal advice.
