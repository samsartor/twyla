#import "/templates/home.typ": *
#set document(
  title: "test_site",
  description: "twyla integration fixture — not a real site.",
)
#show: home-template

This is twyla's integration fixture. It exercises engine primitives a
real port might not: cross-document post enumeration, the raw-html
resolution pass, heading slugs, and intra-document anchor links.

#post-list()
