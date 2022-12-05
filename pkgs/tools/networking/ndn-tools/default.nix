{ lib
, stdenv
, boost
, fetchFromGitHub
, libpcap
, ndn-cxx
, openssl
, pkg-config
, sphinx
, wafHook
}:

stdenv.mkDerivation rec {
  pname = "ndn-tools";
  version = "22.12";

  src = fetchFromGitHub {
    owner = "named-data";
    repo = pname;
    rev = "ndn-tools-${version}";
    sha256 = "sha256-28sPgo2nq5AhIzZmvDz38echGPzKDzNm2J6iIao4yL8=";
  };

  nativeBuildInputs = [ pkg-config sphinx wafHook ];
  buildInputs = [ libpcap ndn-cxx openssl ];

  wafConfigureFlags = [
    "--boost-includes=${boost.dev}/include"
    "--boost-libs=${boost.out}/lib"
    "--with-tests"
  ];

  doCheck = false;
  checkPhase = ''
    runHook preCheck
    build/unit-tests # some tests fail because of the sandbox environment
    runHook postCheck
  '';

  meta = with lib; {
    homepage = "https://named-data.net/";
    description = "Named Data Networking (NDN) Essential Tools";
    license = licenses.gpl3Plus;
    platforms = platforms.unix;
    maintainers = with maintainers; [ bertof ];
  };
}
