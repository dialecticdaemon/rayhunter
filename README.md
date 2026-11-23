> **This branch is an experimental, unofficial port of Rayhunter to the GL.iNet Mudi V2 (GL-E750V2),
> with a prototype that redacts identities and location before recordings are written.**
> It is not part of the EFF's Rayhunter and is not supported by the EFF.
>
> Start with [doc/glinet-mudi.md](doc/glinet-mudi.md), the redaction is described
> [there](doc/glinet-mudi.md#redacting-identities-and-location-experimental). Do not rely on it yet.
> A package for the router is on the
> [releases page](https://github.com/dialecticdaemon/rayhunter/releases).
>
> Tested on GL firmware 4.3.26 and 4.3.30 in my own LTE test cell, not against real IMSI catchers.
> The code was written with the help of an AI assistant (Claude). I tested it on the device and
> am still reviewing it line by line.

# Rayhunter
![Tests](https://github.com/EFForg/rayhunter/actions/workflows/main.yml/badge.svg)

![Rayhunter Logo - An Orca taking a bite out of a cellular signal bar](https://www.eff.org/files/styles/media_browser_preview/public/banner_library/rayhunter-banner.png)

Rayhunter is a project for detecting IMSI catchers, also known as cell-site simulators or stingrays. It was first designed to run on a cheap mobile hotspot called the Orbic RC400L, but thanks to community efforts, it can [support some other devices as well](https://efforg.github.io/rayhunter/supported-devices.html).
It's also designed to be as easy to install and use as possible, regardless of your level of technical skills, and to minimize false positives. 

&rarr;  Check out the [installation guide](https://efforg.github.io/rayhunter/installation.html) to get started.

&rarr; To learn more about the aim of the project, and about IMSI catchers in general, please check out our [introductory blog post](https://www.eff.org/deeplinks/2025/03/meet-rayhunter-new-open-source-tool-eff-detect-cellular-spying). 

&rarr; For discussion, help, or to join the mattermost channel and get involved with the project and community check out the [many ways listed here](https://efforg.github.io/rayhunter/support-feedback-community.html)!

&rarr; To learn more about the project in general check out the [Rayhunter Book](https://efforg.github.io/rayhunter/).

**LEGAL DISCLAIMER:** Use this program at your own risk. We believe running this program does not currently violate any laws or regulations in the United States. However, we are not responsible for civil or criminal liability resulting from the use of this software. If you are located outside of the US please consult with an attorney in your country to help you assess the legal risks of running this program.

*Good Hunting!*
