#import "/templates/home.typ": *
#show: home-template.with(
  title: "test_site",
  description: "twyla integration fixture — not a real site.",
)

This is twyla's integration fixture. It exercises engine primitives a
real port might not: cross-document post enumeration, the raw-html
resolution pass, heading slugs, and intra-document anchor links.

#post-list()
