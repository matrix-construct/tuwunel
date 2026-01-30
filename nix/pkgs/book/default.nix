{
  inputs,

  # Dependencies
  main,
  mdbook,
  stdenv,
}:

stdenv.mkDerivation {
  inherit (main) pname version;

  src = inputs.nix-filter {
    root = inputs.self;
    include = [
      "book.toml"
      "tuwunel-example.toml"
      "CODE_OF_CONDUCT.md"
      "CONTRIBUTING.md"
      "README.md"
      "development.md"
      "debian/tuwunel.service"
      "debian/README.md"
      "arch/tuwunel.service"
      "rpm/tuwunel.service"
      "rpm/README.md"
      "docs"
      "theme"
    ];
  };

  nativeBuildInputs = [
    mdbook
  ];

  buildPhase = ''
    mdbook build -d $out
  '';
}
