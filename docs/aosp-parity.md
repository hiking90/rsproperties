# AOSP property 기능 대조

rsproperties가 Android property system(bionic client, libbase/libcutils
helper, init property service)과 비교해 빠뜨린 기능과 동작 차이를 기록한다.

- 비교 기준: 로컬 AOSP checkout `android-17.0.0_r1`
  (`~/Workspace/aosp`, sparse checkout이므로 `git show HEAD:<path>`로 읽음).
  - bionic: `libc/include/sys/system_properties.h`,
    `libc/system_properties/{system_properties,prop_area,contexts_serialized}.cpp`
  - libbase: `system/libbase/{properties.cpp,parsebool.cpp}`,
    `include/android-base/parseint.h`
  - libcutils: `system/core/libcutils/properties.cpp`
  - init: `system/core/init/property_service.cpp`
- 지원 범위: client는 Linux와 Android, `rsproperties-service`는 Linux 전용,
  Android 지원 API level은 28–37이다(API 37 동작은 AOSP `android-17.0.0_r1`
  source 기준으로 구현했고, API 37 emulator에서 override 조회와 test suite를
  실행했다. C3의 검증 항목 참고). Android에서 service를 쓰는 경로와 macOS는
  비교 대상에서 제외한다.
- 분류
  - (a) Linux emulation이라서 생기는 차이
  - (b) 의도적으로 강화한 동작
  - (c) 의도하지 않은 divergence
  - (d) 기능 누락: AOSP에 있는 API가 없다

## 요약

| # | 영역 | 항목 | 분류 | 상태 |
|---|------|------|------|------|
| C1 | client | property 전체 열거(`__system_property_foreach`) | (d) | 이 branch에서 추가 |
| C2 | client | index로 name·value·serial 읽기(`__system_property_read_callback`) | (d) | 이 branch에서 추가 |
| C3 | client | appcompat override 조회(API 35+) | (c) | 이 branch에서 수정 |
| C4 | client | `WaitForProperty` / `WaitForPropertyCreation` | (d) | 이 branch에서 추가 |
| C5 | client | `GetBoolProperty` / `GetIntProperty` 파싱 규칙 | (d), (c) | 이 branch에서 추가 |
| C6 | client | global reader 초기화 실패를 process 수명 동안 유지 | (c), 재시도 방식은 (a) | 이 branch에서 수정 |
| C7 | client | 한 property의 값 cache와 변경 대기(`CachedProperty`) | (d) | 이 branch에서 추가 |
| S1 | service | `persist.*` / `next_boot.*` 영속화 | (d) | 이 branch에서 추가 |
| S2 | service | property type 검사(`CheckType`) | (c) | 이 branch에서 수정. build.prop 값의 `${...}` 확장도 수정했고, 참조를 load 중인 map에서 찾는 점, 확장 결과의 64 KiB 상한, 호출 하나의 확장 값 합계 4 MiB 상한은 (b) |
| S3 | service | `ctl.*`를 일반 property로 저장 | (c) | 저장은 이 branch에서 수정. handler 호출 순서와 미응답 요청 한도(102)는 init main loop와 같게 구현. 응답은 host가 `ControlReply`로 보내며, init이 fd를 control message와 함께 넘기는 구조와 같다. shutdown 중 `ctl.stop` 응답은 `PowerctlOutcome::ShuttingDown` 또는 `ServiceHandle::stop_handler_calls`로 init과 같아진다. 둘 다 없이 종료하면 거부((b)) |
| S4 | service | 실패 응답 코드가 항상 `PROP_ERROR` | (c) | 이 branch에서 수정 |
| S5 | service | `ro.property_service.version` 미설정 | (c) | 이 branch에서 수정 |
| S6 | service | 변경 알림 hook 없음 | (a) | 검토 대상 아님 |
| S7 | service | properties dir writer lock(`AreaWriterLock`) | (a), 해제 방식은 (b) | lock은 0.6.0부터 있음, 해제 방식은 이 branch에서 수정 |
| S8 | service, builder | prop 파일을 읽지 못했을 때의 반환값 | open 실패로 시작 실패는 (b) | 시작 실패는 0.6.0부터 있음, skip 반환값은 이 branch에서 추가 |
| S9 | service | V1(`PROP_MSG_SETPROP`) 미지원 | (b), owner 지시 | 이 branch에서 변경(0.5.0–0.6.0은 V1을 처리했다) |
| S10 | service | `sys.powerctl`을 일반 property로 저장 | (c), 종료 뒤 거부는 (b) | 이 branch에서 수정. 저장하지 않고 `ServiceOptions::powerctl_handler`로 전달. 대기 command는 init과 같이 하나만 유지 |

## Client

### C1. property 전체 열거

**AOSP.** `__system_property_foreach(cb, cookie)`는 context area마다
`prop_area::foreach`를 호출한다(`contexts_serialized.cpp:151-157`).
`CheckAccessAndOpen()`이 실패한 area, 즉 SELinux가 open을 거부한 area는
건너뛴다. area 안의 순회 순서는 node마다 `left` subtree, 그 node의 `prop`,
`children` subtree, `right` subtree다(`prop_area.cpp:336-360`). 손상된 offset을
만나면 그 area의 순회만 중단하고, `ForEach`는 그 결과를 무시한다. callback은
`prop_info*`만 받으며, 값은 `__system_property_read_callback`으로 읽는다.
libcutils `property_list`는 이 둘을 묶은 wrapper다.

**rsproperties.** trie를 순회하는 코드가 없다. 그래서
`examples/getprop.rs`는 인자 없이 실행하면 "not yet implemented"를 출력하고
exit 1로 끝난다.

**변경.** `SystemProperties::foreach(|PropertyIndex| ...)`를 추가한다.
- context마다 node read lock을 잡은 상태에서 entry offset을 `Vec`에 모은다.
  lock을 놓은 뒤에 callback을 호출한다. 같은 thread에서 같은 `RwLock`의 read
  lock을 다시 잡으면 std 문서상 panic이나 deadlock이 날 수 있다. 이렇게
  하면 callback 안에서 `read_index_with` 같은 다른 읽기 API를 호출해도
  read lock을 두 번 잡지 않는다. 그 대가로 context마다 `Vec` 할당이 한 번
  생긴다. 이 API는 진단용이고 hot path가 아니므로 이 비용을 감수한다.
- 순회 순서는 bionic과 같다. 재귀 대신 explicit stack을 쓰고, 방문한 node
  수가 data 영역에 들어갈 수 있는 node 수를 넘으면 cycle로 보고
  `Error::FileValidation`을 반환한다.
- area를 열 수 없는 context는 bionic처럼 건너뛴다. init 단계에서 손상으로
  빠진 context slot도 건너뛴다.
- bionic `ContextNode::CheckAccessAndOpen`(`context_node.cpp:62-68`)은 open이
  실패하면 `no_access_`를 기록하고, 그 뒤의 `ForEach`는 그 area를 다시 열지
  않는다. 이름 조회 경로(`GetPropAreaForName`)는 접근마다 SELinux audit를 남기도록
  매번 다시 연다. 이 crate도 `foreach` 전용 flag(`ContextNode` `foreach_no_access`)를
  두고, 이름 조회는 매번 다시 연다. 다른 점(b): flag는 권한 오류(`EACCES`,
  `EPERM`)일 때만 기록한다. Linux에서는 이 crate의 service가 reader가 시작된 뒤에
  area 파일을 만들 수 있으므로, 파일이 없다는 오류까지 기록하면 그 instance의
  `foreach`는 나중에 생긴 area를 끝내 보지 못한다.
- bionic과 다른 점(b): 열린 area 안에서 trie 손상을 만나면 순회를 계속하지
  않고 `Err`를 반환한다. 이 crate는 손상과 부재를 구분한다는 정책을 따른다.
  손상된 context보다 앞서 순회한 context의 index는 이미 callback에 전달된
  상태다.
- `SystemProperties::list(|name, value| ...)`를 추가한다. libcutils
  `property_list`(`libcutils/properties.cpp:103-115`)에 대응하며, `foreach`의
  index마다 값을 읽어 callback에 넘긴다. 열거 대상, 순서, 건너뛰는 area는
  `foreach`와 같다.
  - 이름과 값을 호출 1회당 `String` 두 개에 복사하고, node read lock을 놓은
    뒤에 callback을 호출한다. `property_list`는 `read_callback` 안에서, 즉
    값을 읽는 도중에 callback을 호출한다. 이 crate에서 그렇게 하면 callback이
    같은 instance의 read API를 호출할 때 같은 `RwLock`의 read lock을 두 번
    잡는다. 복사 비용은 property마다 `memcpy` 두 번이고, buffer를 재사용하므로
    할당은 값 길이가 늘어날 때만 생긴다.
  - 다른 점(a): UTF-8이 아닌 byte는 U+FFFD로 바꿔 넘긴다
    (`String::from_utf8_lossy`). `property_list`는 C 문자열을 그대로 넘기지만
    callback 인자가 `&str`이므로 같은 방식은 쓸 수 없다. `Error::Utf8`로
    순회를 끝내면 그런 값 하나 때문에 나머지 property를 열거하지 못한다.
    건너뛰면 호출자가 그 property의 존재를 알 수 없다. 값을 byte 그대로
    받으려는 호출자를 위한 API는 두지 않았다.
  - 그 밖의 read 오류는 순회를 끝내고 `Err`로 반환한다(trie 손상과 같은 규칙).
  - 검증: `client_parity_tests::list_matches_foreach_and_allows_reentry`(Linux,
    writer와 reader instance 양쪽). UTF-8이 아닌 값을 쓰는 writer가 없어
    U+FFFD 치환 경로는 test하지 못했다.
- `getprop` 예제는 인자가 없으면 AOSP `getprop`(toolbox,
  `system/core/toolbox/getprop.cpp:54`의 `std::sort`)처럼 이름순으로
  `[name]: [value]`를 출력한다. 열거는 `list`를 쓴다.
  - 다른 점(b): 열거가 실패하면 이 예제는 exit 1로 끝난다.
    `system_properties()`가 `Err`이면 stderr에 오류만 출력하고, `list`가
    `Err`이면 그때까지 모은 property를 출력한 뒤 stderr에 오류를 출력한다.
    AOSP `PrintAllProperties`는 `__system_property_foreach`의 반환값을 쓰지
    않고(`getprop.cpp:41`) 모은 항목만 출력하며, `getprop_main`은 그 뒤에
    0을 반환한다(`:149-152`). 그래서 AOSP `getprop`은 area를 하나도 읽지
    못해도 출력 없이 exit 0으로 끝난다. 이 crate는 손상과 부재를 구분한다는
    정책(위 `foreach` 항목)에 따라 실패를 exit code로 알린다.
- 검증: API 36 emulator의 adb shell(SELinux `shell` domain)에서 이 예제의
  출력과 toolbox `getprop` 출력이 531줄 모두 일치했다. Linux에서는
  `example_service` + `setprop`으로 넣은 4개 property(`ro.` long value
  포함)가 모두 출력되었다.

### C2. index로 name·value·serial 읽기

**AOSP.** `__system_property_read_callback(pi, cb)`는 `prop_info*`를 받는다.
callback에는 한 seqlock 구간에서 검증한 `(name, value, serial)`이 함께
전달된다(`system_properties.cpp:261-280`). `ro.` property는 serial을
relaxed load로 한 번 읽고 값을 복사하지 않는다. mutable property는
`ReadMutablePropertyValue`가 반환하는 serial을 넘긴다. 이 serial은 값을
읽은 iteration의 serial이며, dirty bit가 켜져 있을 수 있다(그때 값은 dirty
backup slot에서 읽은 이전 값이다). libbase `WaitForProperty`와 `CachedProperty`가
이 함수를 쓴다.

**rsproperties.** `read_with(name, f)`는 이름을 받아 호출할 때마다 trie
lookup을 다시 한다. callback은 value만 받는다. `PropertyIndex`로 값을 읽는
경로가 없고, value와 serial을 같은 seqlock 구간에서 얻을 수도 없다.

**변경.** `SystemProperties::read_index_with(PropertyIndex, |name, value, serial| ...)`를
추가한다.
- long 여부는 entry 자신의 이름으로 판정한다. bionic `ReadCallback`이
  `pi->name`으로 판정하는 것과 같다. 기존 `read_with`는 lookup에 쓴 이름으로
  판정하지만, index로 읽을 때는 lookup 이름이 없다.
- serial 의미는 bionic과 같다. mutable property의 serial에는 dirty bit가
  켜져 있을 수 있다. 이 serial을 `wait(idx, Some(serial), ..)`에 넘기면 같은
  update의 clean store를 변경 한 번으로 보고한다.
- 다른 instance의 index는 `Error::InvalidArgument`로 거부한다. `update`와
  같은 규칙이다.

### C3. appcompat override 조회 (API 35+)

**AOSP (API 37, `android-17.0.0_r1`).** `SystemProperties::Init`은
`BIONIC_APPCOMPAT_OVERRIDE` 환경변수가 있으면(값은 보지 않는다)
`use_appcompat_override_`를 켠다(`system_properties.cpp:80-82`). 이 flag가
켜진 프로세스에서 `Find(name)`은 `ro.appcompat_override.<name>`을 먼저 찾는다.
그 이름의 area가 없거나 entry가 없으면 원래 이름을 찾는다(`:168-197`).
override entry는 일반 context area에 있다. 쓰기 경로인 `AreaInit`은 이 flag를
켜지 않는다. 환경변수는 zygote가 `__system_properties_zygote_reload()`
(`EnableOverrides`, `:145-152`)를 호출할 때 `putenv`로 설정하고, 자식
프로세스가 그대로 물려받는다.

**AOSP (API 35–36, `android-15.0.0_r1` ~ `android-16.0.0_r4`).** `Find`에
prefix 조회가 없고(`android-16.0.0_r4` `system_properties.cpp:162-174`),
환경변수도 읽지 않는다. override는 별도 directory와 bind mount로 적용한다.
- init은 `/dev/__properties__/appcompat_override` directory를 만든다
  (`property_service.cpp:1296-1302`, `WRITE_APPCOMPAT_OVERRIDE_SYSTEM_PROPERTIES`
  build 조건).
- writer의 `AreaInit`은 이 directory가 있으면 두 번째 `ContextsSerialized`로
  연다(`:123-135`). `Add`는 모든 property를 이 directory의 area에도 같은
  이름으로 쓴다. `ro.appcompat_override.<name>`은 prefix를 뗀 `<name>`으로
  쓰고, `<name>`이 이미 있으면 그 entry의 value만 덮어쓴다(`:383-409`). 이때
  entry의 serial(value 길이 포함)은 바꾸지 않는다.
- zygote는 대상 app process의 mount namespace에서 이 directory를
  `/dev/__properties__` 위에 bind mount한 뒤
  `__system_properties_zygote_reload()`를 호출한다(frameworks/base
  `android-16.0.0_r1` `com_android_internal_os_Zygote.cpp:1820-1834`). 이
  version의 함수는 `Reload(false)`로 context를 다시 연다
  (`system_property_api.cpp:134-137`).

**mechanism이 바뀐 commit.** bionic `d355473e2`(2026-01-29, "Fix appcompat
override mechanism in system_properties")가 `Find`의 prefix 조회와
`EnableOverrides`를 추가했고, `3bcd5082d`(2026-03-05, "Update appcompat
override for getprop checks")가 환경변수를 추가했다. 두 commit을 포함하는
android-1x tag는 `android-17.0.0_r1`뿐이다. android-16 tag는 r1~r4가 전부이고,
`android-16.0.0_r4`와 branch `android16-qpr2-release`, `android16-release`,
`android16-security-release`의 `system_properties.cpp`에는
`BIONIC_APPCOMPAT_OVERRIDE`가 없다. `android-17.0.0_r1`의 zygote는 bind
mount를 하지 않고 reload 함수만 호출한다(`:1842-1847`). init과 writer는
17에서도 `appcompat_override` directory를 만들고 쓴다
(`property_service.cpp:1328-1334`, `system_properties.cpp:128-139`).

**zygote의 호출 조건.** `SpecializeCommon`은 `mount_sysprop_overrides`가 true일
때만 위 함수를 호출한다(16 `:1964-1967`, 17 `:1978-1981`). 이 값은
`ProcessList.java`가 정한다. DeviceConfig `app_compat` namespace의
`appcompat_sysprop_override_pkgs` 목록에 process의 package가 있으면 true다
(16 `:2519-2529`). 17은 여기에 userdebug/eng build 조건을 더한다
(`:2603-2614`).

**실측.**
- API 36 emulator(`BE2A.250530.026.D1`):
  - libc.so에 `ro.appcompat_override.` 문자열은 있고
    `BIONIC_APPCOMPAT_OVERRIDE` 문자열은 없다.
  - root로 `debug.rsp.ac=orig`, `ro.appcompat_override.debug.rsp.ac=ovr`를
    설정하고 `BIONIC_APPCOMPAT_OVERRIDE=1 getprop debug.rsp.ac`를 실행하면
    toolbox `getprop`(bionic)은 `orig`를 출력한다.
  - `/dev/__properties__/appcompat_override`가 있고 파일은 355개다
    (`/dev/__properties__`는 이 directory를 포함해 356개).
  - 새 mount namespace에서 이 directory를 `/dev/__properties__` 위에 bind
    mount하면 toolbox `getprop`과 이 crate의 `getprop` example이 모두 override 값을
    읽는다(`debug.rsp.ac` → `ovr`, 원래 값보다 긴 `ro.*` override와 짧은
    `ro.*` override 포함). `ro.appcompat_override.debug.rsp.ac`라는 이름은 이
    namespace에서 둘 다 찾지 못한다.
  - mutable property의 override가 원래 값보다 길면 결과가 다르다.
    `debug.rsp.grow=ab`, override `abcdefgh`에서 toolbox `getprop`은 `abc`, 이 crate는
    `abcdefgh`를 출력한다. bionic `ReadMutablePropertyValue`는 serial의 길이로
    `len + 1` byte만 복사하는데(`:184-198`), 위 `Add`가 serial을 바꾸지 않아서
    길이가 원래 값 기준이다. 복사한 buffer에는 NUL이 없으므로 bionic 출력은
    정해진 값이 아니다. 이 crate는 NUL까지 읽는다. `ro.*`는 bionic
    `ReadCallback`도 NUL까지 읽으므로 차이가 없다.
  - zygote 경로는 이 image에서 동작을 확인하지 못했다.
    `device_config put app_compat appcompat_sysprop_override_pkgs
    com.android.settings` 뒤에 Settings를 시작하면 fork된 process가
    SIGSEGV(fault addr 0x60)로 종료한다(test property 없이 재부팅한 뒤에도
    같다). 목록에 없을 때는 정상 시작하고 그 process의 mountinfo에 bind
    mount가 없다. crash 원인은 확인 필요다.
- API 37 emulator(`CE2A.260420.019`, userdebug, `ro.build.version.sdk=37`):
  - libc.so에 `BIONIC_APPCOMPAT_OVERRIDE` 문자열이 있다.
  - 같은 두 property를 설정하면 toolbox `getprop`과 이 crate 모두 환경변수가 없을 때
    `orig`, 있을 때(빈 값 포함) `ovr`를 출력한다. override entry가 없는
    이름(`ro.build.version.release`)은 둘 다 원래 값을 출력한다.
  - `/dev/__properties__/appcompat_override`가 있다(파일 384개).

**rsproperties.** bionic의 override 조회 분기에 해당하는 코드가 없다.
override가 켜진 프로세스에서는 같은 property에 대해 libc
`__system_property_get`과 rsproperties가 서로 다른 값을 돌려줄 수 있다.

**변경.** read-only instance(`SystemProperties::new`, 전역 reader)를 만들 때
환경변수를 한 번 읽어 flag로 저장한다. flag가 켜져 있으면 `find`와
`read_with`가 bionic처럼 override 이름을 먼저 조회한다.
- Android에서는 `ro.build.version.sdk`가 37 이상일 때만 환경변수를 따른다.
  API 35–36에서 환경변수를 따르면, 같은 프로세스의 libc가 읽지 않는 override
  값을 이 crate만 읽게 된다(위 실측).
- API 35–36의 bind mount 방식에는 별도 코드가 필요 없다. 이 crate도
  `/dev/__properties__` 경로를 열므로, bind mount된 namespace에서 만든
  reader는 libc와 같은 파일을 읽는다(위 실측). 조건은 reader를 bind mount
  뒤에 만드는 것이다. bind mount 전에 mmap한 area는 이전 파일을 계속
  가리키고, 이 crate에는 bionic `Reload`에 해당하는 API가 없다. zygote가
  specialize하기 전에 이 crate의 reader를 만드는 구성(zygote preload)은
  지원 대상으로 보지 않는다.
- override 조회는 어떤 오류든 원래 이름 조회로 넘어간다. bionic도 area를
  열 수 없거나(`pa == nullptr`) entry가 없으면 넘어간다. 오류를 그대로
  반환하면 override context를 열 권한이 없는 프로세스에서 모든 읽기가
  실패한다.
- writer instance(`new_area`, `new_area_locked`)에는 적용하지 않는다.
  bionic `AreaInit`과 같다.
- Linux에서는 API level 조건 없이 환경변수만 본다. 기준은 현재 bionic이며,
  Linux에서는 사용자가 직접 설정하지 않는 한 이 변수가 없다.
- 검증: API 36 emulator에서 수정 전 binary는 환경변수가 있으면 `ovr`를
  읽어 toolbox `getprop`(`orig`)과 달랐다. 이 결과에 따라 API 37 조건을 추가했다.
  API 37 emulator(`rsbinder-a17`, android-37.0 google_apis)에서는 수정 후
  binary가 toolbox `getprop`과 같은 값을 읽는다(위 실측). 이 AVD는 `-memory 4096`으로
  띄워야 한다. AVD 설정값인 1536M에서는 guest가 kernel panic으로 재부팅을
  반복해서 adb가 `offline`과 `device`를 오간다.
- API 37 test suite: 같은 image에서 `envsetup.sh`의 `ndk_prepare`,
  `ndk_build`, `ndk_sync`, `ndk_test`, `ndk_test_shell`을 차례로 실행했다.
  test binary 18개의 test 74개가 root(`u:r:su:s0`)와 shell user
  (`u:r:shell:s0`) 실행에서 모두 통과했다. `android-test.yml`의 절차대로 만든
  AVD(`avdmanager create avd -k "system-images;android-37.0;google_apis;x86_64"`,
  emulator option `-wipe-data -memory 4096 -partition-size 2048`)에서도 결과가
  같다. 로컬 조건은 system image revision 6, emulator 36.5.11, NDK r28b이다.
- GitHub runner의 API 37 job: `android-test.yml` run 43214204108(2026-11-12,
  commit `56e7286`)에서 API 28, 30, 34, 36, 37 job이 모두 통과했다. 각 job에서
  test binary 18개의 test 74개가 root와 shell user 실행에서 통과했다. API 37
  job의 조건은 system image revision 8(`CE2A.260420.051.A3`), emulator
  37.2.13, NDK 27.2.12479018이다. workflow는 `workflow_dispatch`로만 시작하고
  `sdkmanager`가 받는 최신 revision을 쓰므로, 이후 run의 image revision은
  이 값과 다를 수 있다.
  - `sdkmanager`에는 `android-37` package가 없고 `android-37.0`이 있다.
    `ro.build.version.sdk`는 37이다. workflow의 matrix는 `api-level: 37`에
    `platform: android-37.0`을 따로 지정한다.

### C4. `WaitForProperty` / `WaitForPropertyCreation`

**AOSP** (`libbase/properties.cpp:246-292`).
- `WaitForPropertyCreation(key, timeout)`: `__system_property_find`가
  `nullptr`이면 global serial이 바뀔 때까지 기다린 뒤 다시 찾는다.
- `WaitForProperty(key, expected, timeout)`: 먼저 생성을 기다린다. 그다음
  `read_callback`으로 값과 serial을 함께 읽고, 값이 `expected`와 같으면
  `true`를 반환한다. 다르면 그 serial을 넘겨 `__system_property_wait`로
  기다린다. 두 함수 모두 시작 시각부터 잰 전체 timeout을 지킨다.

**rsproperties.** serial 단위 `wait`만 있다. 같은 동작을 사용자가 직접
조합해야 한다.

**변경.** `SystemProperties::wait_for_creation(name, timeout)`과
`wait_for_value(name, expected, timeout)`를 추가한다. timeout은
`Option<Duration>`이며 `None`은 무기한이다.
- `wait_for_creation`은 `Ok(Some(PropertyIndex))`를 반환한다. libbase는
  `bool`을 반환하지만, index를 돌려주면 호출자가 `find`를 다시 하지 않아도
  된다.
- libbase는 timeout과 futex 실패를 구분하지 않는다. 이 crate는 futex 실패와
  lookup 실패를 `Err`로 반환하고, timeout만 `Ok(None)` / `Ok(false)`로
  반환한다(b). `wait`도 같은 형태로 바꾼다. timeout은 `Option<Duration>`이고
  결과는 `Result<Option<u32>>`다. bionic `__system_property_wait`의 `bool`도
  실패를 timeout과 구분하지 않는다.
- `wait`의 `old_serial`은 bionic `__system_property_wait`와 같이 필수
  인자(`u32`)다. 0.6.0은 `Option<u32>`였고 `None`이면 `wait` 안에서 현재
  serial을 읽었으므로, 호출 전에 일어난 변경은 반환하지 않았다. bionic에는
  이 형태가 없다.
- bionic이 deprecated로 둔 `__system_property_wait_any(old_serial)`
  (`system_properties.h:125`)은 `Wait(nullptr, old_serial, &new_serial,
  nullptr)`다. 대응하던 `wait_any()`는 삭제했고, `wait(None, old_serial,
  None)`이 같은 호출이다. 이때 `old_serial`은 `area_serial()`
  (`__system_property_area_serial`, `system_properties.h:150`)에서 얻는다.
  0.6.0의 `wait_any()`는 인자가 없어 bionic 함수와 signature가 달랐다.
- 값 비교는 byte 단위로 한다. 값이 UTF-8이 아니면 `expected`와 같지 않은
  것으로 보고 계속 기다린다.

### C5. bool/int 파싱 규칙

**AOSP.**
- libbase `ParseBool`(`parsebool.cpp:23-31`): `1 y yes on true`는 true,
  `0 n no off false`는 false다. 대소문자를 구분하고 공백을 제거하지 않는다.
  그 밖의 값과 빈 값이면 `GetBoolProperty`가 default를 반환한다. libcutils
  `property_get_bool`도 같은 집합을 받는다.
- libbase `ParseInt`(`parseint.h:106-131`): 앞쪽 C `isspace` 문자를 건너뛴다.
  그 위치가 `0x`/`0X`로 시작하면 16진수, 아니면 10진수로 `strtoll`을
  호출한다. 숫자 뒤에 남은 문자가 있으면 실패한다. `-0x10`처럼 부호 뒤에
  `0x`가 오면 10진수로 읽으므로 실패한다. `ParseUint`(`:36-77`)는 `-`로
  시작하면 거부하고, `+`는 받는다.
- libcutils `property_get_int32/64`(`properties.cpp:53-75`): `strtoimax(.., 0)`를
  쓰므로 8진수(`010` = 8)를 받고, 숫자 뒤에 남은 문자(`12abc` = 12)도
  허용한다. 소스 주석은 8진수 허용을 "unwisely"라고 적는다.

**rsproperties.** `get::<i32>`는 Rust `FromStr`을 쓴다. `" 5"`와 `"0x10"`은
실패하고 `get_or`은 default를 반환한다. `"010"`은 10이므로 libbase와
같고 libcutils와 다르다. bool은 `"true"`/`"false"`만 받는다. 이 차이 중
`GetBoolProperty`가 없다는 점만 문서에 있었다.

**변경.**
- 공개 module `parse`에 `parse_bool(&str) -> Option<bool>`과
  `parse_int::<T>(&str) -> Option<T>`를 추가한다. 둘 다 libbase 규칙을 따른다.
  `T`는 sealed trait `parse::Integer`로 제한하며, libbase가 instantiate하는
  `i8..i64`, `u8..u64`에 `isize`, `usize`를 더한다.
- 전역 함수 `get`, `get_or`, `get_or_else`의 bound를 `FromStr`에서 trait
  `FromProperty`로 바꾼다. `bool`은 `parse_bool`, 정수는 `parse_int`,
  `String`은 값 그대로 읽는다. `get_or(name, false)`가 `GetBoolProperty`에,
  `get_or(name, 0)`이 범위 인자를 생략한 `GetIntProperty` /
  `GetUintProperty`에 대응한다.
- 처음에는 `get::<T>`의 `FromStr` 동작을 유지하고 `get_bool_or` /
  `get_int_or`를 따로 두었다. 이 구성에서는 `get_or("ro.debuggable", false)`가
  compile되고 값 `1`에 대해 default를 반환하므로, 함수 이름을 잘못 고른
  호출부를 compiler도 test도 찾지 못한다. 그래서 타입 하나에 규칙 하나만
  남기고 두 함수를 제거했다(둘 다 0.6.0에 없던 함수다).
- 0.6.0 호출자의 결과가 달라지는 입력은 `FromStr`이 거부하던 값뿐이다.
  `FromStr`이 받는 입력을 `FromProperty`가 같은 값으로 읽는지는
  `from_property.rs`의 `accepts_what_from_str_accepted` test가 확인한다.
- `f32`/`f64`는 `FromProperty`를 구현하지 않는다. libbase에 float getter가
  없어 따를 규칙이 없다. `SystemProperties::read_with`에 `str::parse`를 넘겨
  읽는다.
- libcutils 규칙(8진수, 뒤에 남은 문자 허용)은 따르지 않는다. libbase가 더
  나중에 나온 API이고, libcutils 소스도 8진수 허용을 결함으로 적는다.

### C6. global reader 초기화 실패 — (c), 재시도 방식은 (a)

**AOSP.** bionic `SystemProperties::Init`(`system_properties.cpp:65-86`)은
이미 초기화되어 있으면 `ResetAccess()` 후 `true`를 반환한다. 초기화에
실패하면 `initialized_`를 거짓으로 남기므로 다음 `Init` 호출이 다시
시도한다. read 경로는 다시 시도하지 않는다. Android에서는 libc가 process
시작 시 `Init`을 호출하고, 그 시점에 property area가 이미 있다.

**rsproperties.** global reader(0.6.0의 `try_system_properties`, 현재
`system_properties`)는 첫 시도의 결과를
`OnceLock<Result<..>>`에 저장했다. 실패도 저장되므로, service보다 먼저
시작한 client는 첫 read가 실패하면 process가 끝날 때까지 모든 read가
실패했다. 원인은 `OnceLock::get_or_try_init`(Rust issue 109737)이 unstable이라
결과 전체를 저장한 데 있다. 실패를 유지하는 동작을 목표로 고른 commit은 없다.
그 결과 이 상태의 client는 `ro.property_service.version`도 읽지 못해, protocol
version을 env var나 기본값으로만 골랐다.

**변경.**
- 성공한 reader만 `OnceLock`에 저장하고, 생성 구간은 별도 `Mutex`로 막아 한
  thread만 open과 mmap을 한다. 실패하면 cell이 빈 채로 남으므로 다음 접근이
  다시 연다(a: 이 crate의 service는 client보다 늦게 시작할 수 있다). area
  파일이 없을 때 다음 접근에서 다시 여는 기존 정책(`ContextNode::property_area`)과
  같다.
- 실패 상태에서는 호출마다 `property_info` open이 한 번 이상 일어난다.
  `system_properties`는 자신의 log 줄을 첫 실패에만 `error`로, 이후는
  `debug`로 남긴다. 그 함수가 호출하는 `SystemProperties::open`은 실패할
  때마다 `Failed to load contexts from …`를 `error`로 남긴다.
- bionic처럼 명시적 호출에서만 다시 시도하는 방식은 택하지 않았다. 그러려면
  API(`reload` 등)가 늘고, caller가 그 호출 시점을 알아야 한다.
- `init`도 이미 설정된 것과 같은 경로를 다시 받으면 성공한다. bionic `Init`이
  재호출을 성공으로 처리하는 것과 같다. 다른 경로는 `AlreadyInitialized`다.
  bionic은 경로를 바꿀 수 없으므로 이 경우에 대응하는 동작이 없다.

### C7. `CachedProperty` — (d)

**AOSP** (`libbase/properties.cpp:294-370`). `CachedProperty`는 `prop_info*`,
마지막으로 읽은 property serial, 값 buffer(`PROP_VALUE_MAX`)를 보관한다.
- `Get(bool* changed)`: `prop_info*`가 없으면, global serial이 지난 조회 때와
  다를 때만 `__system_property_find`를 다시 호출한다. `prop_info*`가 있으면,
  `__system_property_serial`이 보관한 serial과 다를 때만 `read_callback`으로
  값과 serial을 다시 읽는다.
- `WaitForChange(timeout)`: `prop_info*`가 없으면 `WaitForPropertyCreation`으로
  생성을 기다린다. 있으면 보관한 serial을 넘겨 `__system_property_wait`를
  호출한다. 그 뒤 `Get`을 호출해 값을 반환한다. timeout이면 `nullptr`를
  반환한다.

**rsproperties.** 대응하는 타입이 없다. property 하나의 변경을 계속 읽으려면
호출자가 `find`, `read_index_with`, `wait`를 loop로 묶고 index와 serial을 직접
보관해야 한다. README의 예제는 이 조합을 잘못 썼다. `find`가 `None`을
반환하면 기다리지 않고 끝났고, 변경 1회를 값 확인 없이 "boot completed"로
출력했다.

**변경.** `SystemProperties::watch(name) -> PropertyWatcher<'_>`를 추가한다.
`value()`가 `Get`에, `wait_for_change(timeout)`이 `WaitForChange`에 대응한다.
- `value()`의 반환 타입은 `Result<Option<&str>>`이고, property가 없으면
  `Ok(None)`이다(b). libbase `Get`은 property가 없을 때 빈 문자열을 반환하므로
  호출자가 빈 값과 부재를 구분하지 못한다.
- `wait_for_change`의 반환 타입은 `Result<bool>`이다. timeout은 `Ok(false)`,
  lookup 실패와 futex 실패는 `Err`다(b, C4와 같은 규칙).
- update 1회를 변경 1회로 보고한다(b). writer는 dirty serial, 값, clean serial
  순서로 store하고 futex wake는 clean store 뒤에 한다. `__system_property_wait`는
  dirty 구간에 진입하면 dirty serial을 반환한다. libbase는 이때 `Get`을 호출해
  backup slot의 이전 값과 dirty serial을 보관하므로, 다음 `WaitForChange`가 같은
  update의 clean store에서 한 번 더 반환한다. watcher는 dirty bit를 지운 serial을
  보관한다. `wait`가 반환한 뒤 읽은 serial에서 dirty bit를 지운 값이 보관한
  serial과 같으면 그 dirty serial부터 다시 기다린다.
- 같은 값을 다시 쓴 update도 변경으로 보고한다(libbase와 같다). serial은
  update마다 증가하고, area에는 이전 값이 남지 않으므로 비교할 대상이 없다.
- 변경을 queue에 쌓지 않는다(libbase와 같다). 두 호출 사이에 일어난 update
  여러 번은 변경 1회이고, 값은 마지막 값이다.
- 값 buffer는 watcher마다 `String` 하나다. libbase는 `ro.` 값을 복사하지 않고
  mapping 안의 pointer를 반환한다. 이 crate의 read는 node read lock을 잡은
  동안에만 유효한 `&str`을 넘기므로 `ro.` 값도 복사한다. `ro.` property는
  serial이 바뀌지 않으므로 복사는 한 번이다.
- UTF-8이 아닌 값은 `Error::Utf8`이다. 이때 watcher는 serial을 기록하지
  않는다. 그래서 property가 다시 바뀔 때까지 `value()`는 호출마다 같은 오류를
  반환하고, `wait_for_change`는 기다리지 않고 같은 오류를 반환한다. 오류를
  한 번만 보고하려면 watcher가 오류 상태를 따로 보관해야 하는데, 그 경로를
  재현할 test 수단(UTF-8이 아닌 값을 쓰는 writer)이 없어 넣지 않았다.
- free 함수 `rsproperties::watch`는 두지 않는다. `system_properties()?.watch(name)`이
  같은 호출이고, global reader를 열지 못한 오류(`Error::Init`)가 `watch`가 아닌
  `system_properties()`에서 반환된다.
- 검증: `property_watcher.rs`의 unit test(Linux, `builder` feature). dirty store는
  writer mapping의 serial word에 bit 0을 직접 세워 재현한다.
  `tests/property_change_wait_tests.rs`의
  `test_watcher_follows_creation_and_update`는 Android에서 init을 상대로 생성
  1회와 update 1회를 확인한다.

## Service (Linux)

각 절의 `**변경 전(main).**` 문단은 이 branch 이전(main) 상태를 적는다. 그
문단이 인용하는 rsproperties 줄 번호는 main 기준이다.

### S1. `persist.*` / `next_boot.*` 영속화 — (d)

init은 set이 성공하면 이름이 `persist.` 또는 `next_boot.`로 시작하는 값을
persistent store에 기록한다(`property_service.cpp:414-423`). 부팅할 때 다시
load하고 `ro.persistent_properties.ready=true`를 설정한다(`:1462`).

**변경 전(main).** rsproperties-service는 이 값을 저장하지 않으므로, 재시작하면
모두 잃는다.

**AOSP 세부 동작** (`persistent_properties.cpp`).
- 형식은 proto2 `PersistentProperties { repeated PersistentPropertyRecord
  { optional string name = 1; optional string value = 2; } properties = 1; }`다.
- 쓰기(`WritePersistentProperty`): 파일을 다시 읽고, 같은 이름의 첫 record를
  고치거나 끝에 추가한다. 값이 같으면 쓰지 않는다. 파일을 읽지 못하면(없음,
  group/world-writable, 파싱 실패 후 삭제) "Recovering persistent properties
  from memory"를 log하고, area에 있는 `persist.*` 값 전체를 record로 모아 같은
  처리를 한다(`persistent_properties.cpp:208-224`, `:231-235`). `PropertySet`은
  area 값을 먼저 바꾼 뒤 이 함수를 호출하므로(`property_service.cpp:405-422`),
  모은 record에는 방금 set한 `persist.<name>`의 새 값이 이미 들어 있다. 그래서
  이 경로에서 `persist.*` set은 파일을 쓰지 않고 반환하고, `next_boot.*` set은
  area의 `persist.*` 값 전체에 그 record를 더해 파일을 쓴다. `<file>.tmp`(0600,
  `O_NOFOLLOW`)에 쓰고 fsync, rename, 디렉터리 fsync 순서로 진행한다.
  실패는 log만 남기고 set은 성공으로 응답한다.
- 읽기(`LoadPersistentProperties`): 남은 `.tmp`를 지우고 파일을 읽는다.
  읽기는 `init/util.cpp`의 `ReadFile`이다. `O_NOFOLLOW`로 열고 `fstat`한 뒤
  group 또는 other write bit가 있으면 "Skipping insecure file" error를
  반환한다. 이 경우 `LoadPersistentPropertyFile`은 파일을 지우지 않고 실패를
  반환한다. 파싱에 실패하거나 `persist.`/`next_boot.`가 아닌 이름이 있으면
  파일을 삭제하고 실패를 반환한다. 실패하면 `LoadPersistentProperties`가
  legacy 경로(`/data/property/persist.*` 파일들)를 읽고, 그 결과를
  `WritePersistentPropertyFile`로 즉시 써서 기존 파일을 대체한다
  (`persistent_properties.cpp:254-272`). legacy 파일이 없으면 record가 없는
  파일이 된다. 따라서 group/world-writable 파일의 record는 그 부팅의 load
  시점에 사라진다. `next_boot.<name>` record는 `<name>`의 값을 바꾸고 제거된 뒤
  파일에 다시 기록된다. 각 record는 `InitPropertySet`으로 설정되므로 type
  검사를 거친다. 그다음 `ro.persistent_properties.ready=true`를 설정한다.

**변경.** `ServiceOptions::persistent_properties_file(path)`로 켠다. 위 규칙을
그대로 따르며, codec은 새 dependency 없이 crate 안에 직접 구현했다(약 150줄).
init과 다른 점은 다음과 같다.
- (a) Android 8 이전의 legacy layout(`/data/property/persist.*` 파일들)은
  읽지 않는다. legacy 경로가 없으므로 읽지 못한 파일(symlink,
  group/world-writable 등)은 load 때 대체되지 않고 다음 persist set 전까지
  남는다. 그 전에 mode를 고치고 재시작하면 record가 다시 load된다. 다음
  persist set에서 이 service는 메모리의 record, 즉 load 뒤에 set된 record만으로
  파일을 다시 쓴다. 같은 상황에서 init은 쓰기 시점에 파일을 읽지 못하므로 area의
  `persist.*` 값으로 record를 모은다(위 쓰기 항목). init에서 이 경로는 load
  시점의 파일 쓰기가 실패했거나, 실행 중에 파일이 사라지거나 mode가 바뀐 경우에
  실행된다.
- (b) load가 일시적 오류(`EINTR`, `EIO`, `EAGAIN`, `ENOMEM`, `ENOBUFS`,
  `EMFILE`, `ENFILE`)로 파일을 읽지 못하면, 첫 persist set이 파일을 쓰기 전에
  다시 읽는다. 읽으면 파일의 record(중복은 load처럼 합침)에 그 run의 set을
  덮어 쓴다. 다시 읽은 record는 area에 적용하지 않는다. init도 쓰기 때 다시 읽은
  record를 area에 적용하지 않는다. 다시 읽어도 일시적 오류이면 그 set은
  메모리에만 두고 파일을 쓰지 않는다. 그러면 다음 set이 다시 시도한다. 이렇게
  하지 않으면 일시적 오류 한 번 뒤의 첫 set이 파일을 그 set 하나로 대체한다.
  권한 오류나 mode 검사처럼 매번 같은 결과가 나는 오류는 다시 읽지 않고 위 (a)와
  같이 처리한다.
- (b) 시작 후에는 record를 메모리에 두고, set마다 메모리를 갱신한 뒤 그
  내용 전체를 파일에 쓴다. 쓸 때마다 파일을 다시 읽지 않는다.
  - init이 다시 읽는 이유는 소스 주석에 적힌 대로 PID 1의 상주 메모리를
    줄이기 위해서다("we rather not keep any data in memory"). 다시 읽기는
    동기화 수단이 아니다. `persistent_properties.cpp`에는 mutex가 없다. init에서
    이 파일 쓰기가 thread 하나에서만 실행되는 것은 init의 검사가 아니라 client가
    고르는 socket에 따른 결과다.
    - thread 구성. `StartPropertyService`는 `property_service_for_system`(0660,
      root:system)과 `property_service`(0666) socket마다 `StartThread`로 thread를
      하나씩 띄우고(`property_service.cpp:1534-1564`), 두 thread 모두
      `handle_property_set_fd` → `PropertySet`을 실행한다. init은 socket에 따라
      이름을 제한하지 않는다. `ro.property_service.async_persist_writes`가
      false(기본값, `:1566-1571`)이면 `PersistWriteThread`가 없으므로 각 thread가
      `WritePersistentProperty`를 직접 호출한다(`:422`). 이 값이 true이면
      `PersistWriteThread` 하나가 queue의 쓰기를 순서대로 실행한다
      (`:1501-1520`).
    - bionic을 거치는 client만 있으면 두 쓰기는 겹치지 않는다. bionic
      `__system_property_set`은 이름이 `sys.powerctl`이고 socket에 쓰기 권한이
      있을 때만 `property_service_for_system`으로 보낸다
      (`system_property_set.cpp:64-73`). `PropertySet`은 이름이 `sys.powerctl`이면
      area 변경과 파일 쓰기를 모두 건너뛴다(`:378-430`). 두 번째 socket을 추가한
      commit `689adfad3`(2023-07-18)의 message가 이 제한을 적고 있다: "since
      writes to properties are not thread-safe, limit use of this second socket
      to just sys.powerctl messages". 이름 제한 없이 socket을 추가한 `90879edee`는
      race condition을 이유로 `be392fc69`에서 revert되었다. API 36 emulator
      image(`sdk_phone64_x86_64`)에서 이 socket 이름을 포함한 파일은 bionic
      `libc.so`, `init`, 정적 link된 `snapuserd`, `plat_file_contexts`뿐이다.
    - 겹치는 조건 1: root 또는 system group process가 bionic을 거치지 않고
      `property_service_for_system`에 V2 `persist.*` set을 보내는 경우. SELinux
      label은 두 socket이 같으므로(`property_socket`,
      `private/file_contexts:182`) 접근을 가르는 것은 DAC뿐이다. API 36
      emulator에서 root shell로 실측했다. `persist.rsprop.a`를
      `property_service`로, `persist.rsprop.b`를 `property_service_for_system`으로
      동시에 set하는 round를 300회 실행하면, 응답이 `PROP_SUCCESS`이고 area 값도
      바뀌었는데 파일에 record가 없는 set이 53건(재실행 63건) 나온다. init
      log에는 같은 수의 "Unable to rename persistent property file: No such file
      or directory"가 남는다. 두 thread가 같은 `.tmp` 경로에 쓰고(`:171`), 읽는
      쪽은 `.tmp`가 있으면 `unlink`하기 때문이다(`:127-134`). 두 set을 모두
      `property_service`로 보낸 대조 실행 300회에서는 빠진 record가 없다.
      이런 client가 실제로 있는지는 GitHub code search
      (`property_service_for_system`, 상위 100건)로 확인했다. 이 socket에
      쓰는 code는 bionic과 그 fork(`topjohnwu/system_properties`, 같은
      `sys.powerctl` 전용 routing)뿐이고, 나머지는 init·sepolicy 사본과 server
      재구현이다. 공개 source와 emulator image 범위에서는 이런 client가 없다.
      비공개 vendor binary는 조사 범위 밖이다. 있더라도 이 service의 동작은
      달라지지 않는다.
    - 겹치는 조건 2: `load_persist_props`가 두 번 실행되는 경우.
      `do_load_persist_props`에는 횟수 guard가 없다(`builtins.cpp:1072-1077`).
      `kLoadPersistentProperties`는 `property_service_for_system` thread가
      처리하고(`:1562`의 `listen_init`, `:1447`), `LoadPersistentProperties`는
      파일을 읽을 때 `.tmp`를 `unlink`하며 staged record가 있으면 파일을 다시
      쓴다(`persistent_properties.cpp:315`). 첫 load 동안에는
      `persistent_properties_loaded`가 false라서 다른 thread의 set이 파일을 쓰지
      않는다. AOSP `rootdir/init.rc`와 API 36·37 emulator image의 rc 파일에서
      이 command를 호출하는 곳은 `init.rc` 한 곳이다. GitHub code search
      (`load_persist_props`, `.rc`, 상위 100건)에서 두 번째 호출이 있는 rc는
      세 부류다. Android 13 이하 `init.rc` 사본의 FDE용
      `on property:vold.decrypt=trigger_load_persist_props`(그 version의
      `do_load_persist_props`에는 횟수 guard가 있었다), recovery 전용
      `init.rc`(TWRP 계열), 비공식 Magisk rc 사본의 `on post-fs-data`다.
      Magisk 공식 tag(v20.4, v21.4, v23.0, v25.2, v26.4, v27.0)와 default
      branch의 `magiskrc.inc`에는 이 호출이 없다.
      Android 14 이후 tree에서 두 번째 호출을 추가한 rc는 찾지 못했다.
      공개 source와 emulator image 범위에서는 두 번째 호출이 없다. 비공개
      vendor tree는 조사 범위 밖이다. 있더라도 이 service의 동작은 달라지지
      않는다.
    - 조건 2의 첫 load 구간에서는 파일 쓰기와 별개로 area 변경이 겹칠 수
      있다. `property_service_for_system` thread가 load한 record를
      `InitPropertySet`으로 적용하는 동안(`:1447-1463`) `property_service`
      thread는 client set을 처리하고, `PropertySet`의 area 변경 구간에는
      lock이 없다. bionic `SystemProperties::Add`와 `prop_area::allocate_obj`는
      writer가 하나라는 전제로 작성되어 있다(`system_properties.cpp:434`의
      주석 "There is only a single mutator", `prop_area.cpp:152-157`의
      `bytes_used_`는 atomic이 아니다). `689adfad3` 이전에는 thread 하나가
      init socket과 client socket을 모두 처리했다. source로만 확인했고
      실측하지 않았다. 이 service는 startup record를 `on_start`에서 적용한
      뒤에 mailbox의 set을 처리하므로 이 구간이 없다.
    - 이 service에서는 두 socket의 set이 모두 `PropertiesService` actor 하나를
      거치므로(아래 "쓰기 순서") 위 두 조건이 생기지 않는다.
  - 메모리 방식에서는 쓰기가 실패한 변경이 메모리에 남아 다음 쓰기에 함께
    기록된다. init 방식은 다음 쓰기 때 파일을 다시 읽으므로, 실패한 변경이
    파일에서 빠진다.
  - 쓰기 순서: `PropertiesService` actor가 message를 하나씩 처리하고 파일
    쓰기를 `handle` 안에서 await하므로, 다음 set은 이전 쓰기가 끝난 뒤에
    시작한다.
  - 실행 중에 다른 program이 파일을 고치면 다음 persist set이 그 내용을
    덮어쓴다. 실행 중에는 파일을 고치지 않는다는 조건을 문서에 적었다.
- (b) 메모리 방식에서는 같은 파일을 쓰는 instance가 둘이면 서로의 record를
  덮어쓴다. `AreaWriterLock`은 properties dir만 보호한다. 그래서 load할 때
  `<file>.lock`(0600, `O_NOFOLLOW`)에 `flock(LOCK_EX|LOCK_NB)`을 잡고 store가
  살아 있는 동안 유지한다. store를 drop할 때 `flock(LOCK_UN)`으로 푼다. fd를
  닫는 것만으로는 같은 process가 만드는 중인 child가 `exec` 전까지 가진 fd
  사본 때문에 lock이 남을 수 있다. `fork`한 child에서 store가 drop되면
  (`load`를 호출한 pid와 현재 pid가 다르면) `LOCK_UN`을 호출하지 않는다. child의
  fd는 parent의 fd와 같은 open file description을 가리키므로, child가 `LOCK_UN`을
  호출하면 parent가 쓰는 중인 파일의 lock이 풀린다. S7의 `AreaWriterLock`과 같은
  규칙이다. 두 번째 instance는 시작에 실패한다
  (`ErrorKind::WouldBlock`). init에는 lock이 없다. device에서는 init이 유일한
  writer이기 때문이다.
- (b) load할 때 같은 이름의 record를 첫 위치로 합치고 마지막 값을 남긴다.
  load는 record를 순서대로 모두 적용하므로 area에 남는 값도 마지막 값이다.
  `update`는 같은 이름의 첫 record만 고친다. 그래서 합치지 않으면 뒤쪽
  중복 record의 이전 값이 다음 시작 때 다시 적용된다. 예를 들어 파일이
  `[a=1, a=2]`일 때 `a=3`으로 set하면 파일은 `[a=3, a=2]`가 되고, 재시작
  후에는 `a=2`가 된다. init이 만든 파일에는 중복이 생기지 않으므로, 이 문제는
  외부에서 만든 파일에서만 나타난다. 합친 결과가 원본과 다르면 파일에
  다시 쓴다.
- (b) 파일의 디렉터리가 없으면 시작을 실패시킨다. init에서는 이 디렉터리가
  항상 있다. store load(lock, 읽기, 정리한 결과의 재기록)는 `property_info`와
  area를 쓰기 전에 실행한다. 그래서 디렉터리가 없거나 다른 service가 lock을
  쥐고 있으면 properties dir은 바뀌지 않은 채로 시작이 실패한다.
- 남은 staged record를 덧붙이는 순서는 init에서는 `unordered_map` 순서라
  정해져 있지 않다. 여기서는 파일에 나온 순서를 따른다.
- (b) 대상 이름이 `persist.`/`next_boot.`로 시작하지 않는 staged record
  (`next_boot.foo` → `foo`)는 load할 때 area에 한 번 적용하고 파일에는 쓰지
  않는다. init은 `foo`를 파일에 쓰고, 다음 load(다음 부팅, 또는 같은 부팅의
  다음 persistent write)가 prefix 검사에 실패해 파일 전체를 지운다.
  device에서는 `property_contexts`가
  `next_boot.*`를 set할 수 있는 주체를 제한하지만, 이 service는
  per-property 인가를 하지 않는다. init처럼 파일에 썼다면 socket에 접근할 수
  있는 client는 누구나 set 한 번과 재시작 두 번으로 모든 `persist.*` 값을
  지울 수 있었다. 여기서는 그 record를 파일에 쓰지 않으므로 그 경로가 없다.
  area에 남는 값은 init과 같다. 이런 record를 area에도 적용하지 않는 방법도 있으나,
  그러면 첫 재시작 후 area 결과가 init과 달라진다.
  - 파일 재기록(`PersistentStore::load`)은 area 생성
    (`init_system_properties_sync`)과 set(`apply_startup_properties`)보다
    먼저 실행된다. init도 파일을 먼저 쓴다
    (`persistent_properties.cpp:315-321`). 재기록 뒤에 `run`이 실패하거나
    그 set이 client set과 같은 검사에서 거부되면, 이 record의 값은 파일에도
    area에도 남지 않는다. 대상 이름이 `persist.`로 시작하는 staged record는
    재기록된 파일에 `persist.<name>`으로 남으므로 다음 시작에서 적용된다.
    init에서는 `foo`가 파일에 남지만 다음 load(다음 부팅, 또는 같은 부팅의
    다음 persistent write)가 파일 전체를 지우므로 그 값이 다시 적용되지
    않는다. 이 조건은 `run` rustdoc의
    "Properties set at startup"에 적었다.
- 손상된 파일은 owner 결정에 따라 init처럼 삭제한다.
- V1(`PROP_MSG_SETPROP`) set은 이 service가 처리하지 않는다(S9). 따라서 V1
  set이 area나 persistent properties file에 반영되는 경로가 없다. init은 V1
  set을 area에만 반영하고 파일에는 기록하지 않는다. init의 기록 조건은
  `socket && persistent_properties_loaded && need_persist`(`:417`)이고, V1은
  `HandlePropertySetNoSocket`(socket 없음)을 거치기 때문이다. API 36
  emulator에서 V1 frame으로 `persist.*`를 set하면 area 값은 바뀌고 파일에는
  record가 생기지 않는다.
  - init의 조건에 `socket &&`가 들어간 것은 `0af9ab330`(2022-10-07, "init:
    Allow persistent properties to be written asynchronously")부터다. V1
    호출부는 그 전에도 socket 없이 호출했으므로, 그 이전 init은 V1
    `persist.*`도 파일에 기록했다. commit message에는 V1에 대한 언급이 없다.
- `run`이 반환되기 전에 도착한 V2 `persist.*` set은 load한 값을 덮어쓰고 파일에도
  기록된다. mailbox에서 기다린 set은 `on_start`가 persistent record까지 적용한
  뒤(`apply_startup_properties`) 처리되기 때문이다. init은 load 전의 set을
  파일에 쓰지 않는다(`persistent_properties_loaded`, `property_service.cpp:414-417`).
  그 뒤 `HandleInitSocket`이 파일의 record마다 `InitPropertySet`을 호출하므로
  (`:1447-1455`), 파일에 record가 있는 이름은 area에도 파일 값이 남는다.

### S2. type 검사 — (c)

**변경 전(main).** `property_contexts`의 type(`int`, `bool`, `enum ...`)은
파싱 단계에서 검증되고 trie에도 직렬화된다(`property_info_serializer.rs:74-87`).
그런데 set
handler(`properties_service.rs:227-239`)는 이름과 길이만 검사한다. init
`CheckPermissions`는 type이 맞지 않으면 `PROP_ERROR_INVALID_VALUE`를
반환한다(`:530-533`). `socket_service.rs:54-63`에 문서화된 access model은
SELinux와 SO_PEERCRED 검사 생략만 다루고, type 검사 생략은 언급하지 않는다.

**AOSP 규칙** (`property_type.cpp` `CheckType`): 빈 값은 항상 통과한다.
`string`은 모두 통과, `bool`은 `true`/`false`/`1`/`0`, `int`/`uint`는 libbase
`ParseInt`/`ParseUint`(64 bit), `double`은 `ParseDouble`(`strtod`, 문자열 전체,
`ERANGE` 거부, 결과가 `[lowest(), max()]` 밖이면 거부하므로 `inf`는 거부하고
NaN은 통과), `size`는 `[0-9]+[gkm]`, `enum a b`는 나열된 값 중 하나여야
한다. 그 밖의 type은 거부한다. `bool`의 허용 집합은 `ParseBool`보다 좁다.

**변경.** `rsproperties-service/src/property_type.rs`에 같은 규칙을
구현했다. type은 새 API `SystemProperties::property_type(name)`(builder)으로
`property_info`에서 읽는다. init과 다른 점: `double`에서 `strtod`가 받는
hex float(`0x1p3`)와 `nan(...)`을 Rust float parser가 받지 않으므로 거부한다.
subnormal 결과(`1e-310`)는 거부한다. bionic `strtod`(gdtoa `strtod.c`)는 십진
지수가 -256 이하이면 `scale`을 설정하고, 마지막에 결과의 exponent field가
0이면 `errno = ERANGE`로 둔다(`:1074-1087`). subnormal 결과는 항상 이 경로를
거친다. device 실측은 하지 않았다.

type 검사는 client set, persistent record(`init_set` → `apply_set`),
build.prop entry에 적용한다.

**build.prop entry.** init `LoadProperties`는 entry마다 `CheckPermissions`를
호출하고(`property_service.cpp:803`), 그 안의 `CheckType`(`:530-533`)이
실패하면 그 entry를 버린다. `PropertyInit`은 `property_info`를 load한
뒤(`:1415`) `PropertyLoadBootDefaults`(`:1429`)를 실행하므로, 이 시점에 type
정보가 있다. 0.6.0의 이 service는 검사 없이 `add`했으므로, `property_contexts`가
`int`로 정한 이름에 build.prop가 `abc`를 주면 `abc`를 저장했다.

이 service도 init과 같은 시점, 즉 entry를 읽을 때 같은 이름의 이전 entry를
대체하기 전에(`:803-811`) 검사한다. 이를 위해 `rsproperties`에 `builder`
feature의 공개 함수 두 개를 추가했다.
- `load_properties_from_file_with(path, filter, properties, accept)`. loader가
  filter, `ctl.*` 제외, 이름 검사를 통과한 entry마다 `accept(name, value)`를
  호출한다. 호출 위치는 init의 `CheckPermissions` 자리이며 map을 바꾸기
  전이다. `accept`가 `false`를 반환한 entry는 `warn` log(파일, 줄 번호,
  이름)를 남기고 버리며, map에 있던 같은 이름의 값은 그대로 둔다. `import`한
  파일의 entry에도 같은 `accept`를 호출한다. `load_properties_from_file`은
  항상 `true`를 반환하는 closure로 이 함수를 호출한다.
- `property_type_in_trie(trie, name)`. `build_trie`가 반환한 byte에서 type을
  읽는다. `SystemProperties::property_type`은 `property_info`를 게시하고 area를
  만든 뒤에만 쓸 수 있는데, 이 service는 입력 파일을 모두 읽은 뒤에
  properties dir에 쓴다(`build_prop_error_leaves_property_info_untouched`).
  그래서 build.prop를 읽는 시점에는 trie byte만 있다. 함수는 header 크기,
  version, header에 기록된 크기를 `PropertyInfoAreaFile::load_path`와 같은
  규칙으로 검사한다. trie record는 4-byte 정렬을 전제로 읽으므로, slice의
  시작 주소가 4-byte 정렬이 아니면 `Vec<u32>`로 복사한 뒤 조회한다. 반환
  타입이 `Option<String>`인 이유가 이 복사 경로다.

`init_system_properties_sync`는 `check_type`을 호출하는 closure를 `accept`로
넘긴다. type이 맞지 않으면 이름과 type을 `error` log로 남긴다. area에 넣는
loop는 type을 다시 검사하지 않는다. init의 `PropertySetNoSocket` loop
(`:1234-1239`)도 `CheckPermissions`를 거치지 않는다.

이 변경으로 `ro.property_service.version`의 결과도 달라진다. AOSP
`plat_property_contexts`는 이 이름을 `exact int`로 정한다. build.prop 값이
`v2`처럼 `int`가 아니면 type 검사에서 entry를 버리므로, build.prop 값
판정(S9)에 도달하지 않고 service가 `2`를 설정한다. init의 결과와 같다. 값이
`1`처럼 `int`이면서 V1을 고르는 경우에는 종전대로 `run`이 실패한다.
`property_contexts`에 이 이름의 type이 없으면 `v2`도 종전대로 `run`이
실패한다.

**값 확장(`ExpandProps`).** init은 값에 `ExpandProps`를 적용하고(`:795`)
확장한 값으로 `CheckPermissions`를 호출하며, 확장에 실패한 entry는
버린다(`:796-800`). 이 crate의 loader는 `import` 경로만 확장하고 값은 파일에
적힌 그대로 저장했다(c). 그래서 값에 `${name}`이 있는 entry는 저장하는 값도,
type을 검사하는 값도 init과 달랐다.

loader가 `ctl.*` 제외 뒤, 이름 검사 앞에서 값을 확장하도록 수정했다. 확장
함수는 `import` 경로에 쓰던 것과 같다(`expand_props`, 종전 이름
`expand_import_path`). 값에 `$`가 없으면 복사 없이 통과한다. 확장에 실패하면
`error` log(파일, 줄 번호, 이름)를 남기고 entry를 버리며, map에 있던 같은
이름의 값은 그대로 둔다. `accept`에는 확장한 값을 넘기므로 service의 type
검사도 확장한 값을 대상으로 한다. init은 실패한 값을 log에 남기지만 이
loader는 남기지 않는다(값을 log에 쓰지 않는 기존 정책).

확장 결과의 길이 상한(b). init `ExpandProps`는 결과 길이를 제한하지 않는다.
init에서는 확장한 값이 같은 load의 다음 줄 조회 대상이 되지 않으므로(아래
표의 마지막 행), 한 줄의 결과는 그 줄의 참조 개수와 area에 이미 있는 값의
길이로 정해진다. 이 loader는 확장한 값을 map에 저장하고 다음 줄이 그 값을
참조하므로, 각 줄이 `${x}`를 여러 번 반복하면 `x`의 길이가 줄마다 그 배수로
커진다. 64 KiB 이하의 줄 세 개(`x=` + 약 65,500 byte, `x=` + `${x}` 16,383회
두 줄)로 두 번째 줄에서 약 1 GB, 세 번째 줄에서 약 16 TB 할당을 요구하고,
할당 실패는 `handle_alloc_error`로 process를 abort한다. 그래서 `expand_props`는
결과가 `MAX_LINE_LEN`(64 KiB, 줄 길이 상한)을 넘으면 그 시점에 확장 실패로
처리한다. 확장에 실패한 다른 경우와 같이 entry를 버리고, `import` 경로라면
그 import를 건너뛴다. 상한을 줄 길이로 정한 이유는 map에 저장되는 값의 상한을
확장 전후로 같게 두기 위해서다. `PROP_VALUE_MAX`(92)는 쓰지 않았다. `ro.`
값은 92 byte 이상도 저장되므로(long property), 그 상한은 init이 저장하는
`ro.` 값을 버린다. 포기한 것은 64 KiB를 넘는 확장 결과다. init은 저장을
시도한다.

확장한 값의 합계 상한(b). 줄 하나의 상한은 map 전체 크기를 제한하지 않는다.
여러 key가 같은 긴 값을 참조하면 key마다 그 값의 사본을 저장하기 때문이다.
root 파일에 `x=` + 약 65,500 byte 한 줄과 약 10 byte짜리 `kN=${x}` 줄 100만
개(파일 약 10 MB)를 두면, 줄마다 결과는 64 KiB 이하지만 map은 약 64 GB를
요구하고 할당 실패로 process가 abort된다. 확장이 없으면 map 크기는 읽은 파일의
byte 수에 비례하는데, 이 경로에서는 입력 1 byte당 약 6,000 byte를 저장한다.
init에서는 참조 대상인 area에 같은 load의 entry가 없으므로 이 증폭이 생기지
않는다. 증폭의 원인은 map 조회(아래 표의 마지막 행)다. 그래서 loader는
`load_properties_from_file` 호출 하나(그 `import` 포함)에서 확장한 값의 길이를
더하고, 합계가 `MAX_EXPANDED_BYTES`(4 MiB)를 넘으면 `MAX_TOTAL_LOADS`와 같이
`Error::LimitExceeded`로 그 호출을 중단한다. 그 전에 저장한 entry는 map에
남는다. service에서는 `run`이 `StartupStage::PropertiesService`로 실패한다.
초과한 entry만 버리고 계속하는 방식은 택하지 않았다. 그 방식에서는 이후의
확장 entry가 줄마다 error log를 남기고 모두 빠지는데도 load가 `Ok`로 끝난다.
4 MiB는 최대 길이 결과 64개에 해당한다. 실제 build에서 `${...}`를 쓰는 값의
합계는 이보다 훨씬 작다고 보았다(기기별 실측은 확인 필요). 상한은 호출 하나에
적용되므로, build.prop 파일마다 호출하는 service에서 map에 든 확장 값의 합계는
파일 수 × 4 MiB까지 커질 수 있다. 포기한 것은 확장한 값의 합계가 4 MiB를 넘는
파일 tree의 load다. init은 이런 tree도 읽는다.

API 37 emulator(`android-17`, userdebug)에서 `/data/local.prop`으로 init의
동작을 실측했다. `local.prop`도 `LoadProperties`를 거친다(`:870-878`).

| 값 | init 결과 | 이 loader |
|---|---|---|
| `${ro.build.type}-x` | `userdebug-x` 저장 | 같음(이름이 map에 있을 때) |
| `${없는 이름:-dflt}` | `dflt` 저장 | 같음 |
| `${없는 이름:-}`, `${없는 이름}` | 버림(`property ... doesn't exist`) | 같음 |
| `a$$b` | `a$b` 저장 | 같음 |
| `a$b` | 버림(`using deprecated syntax`) | 같음 |
| `abc$` | `abc` 저장 | 같음 |
| `${ro.build.type`(닫는 `}` 없음) | 버림 | 같음 |
| `${:-d}` | 버림(`invalid zero-length property name`) | 같음 |
| `first` 뒤에 같은 이름으로 `${없는 이름}` | `first` 유지 | 같음 |
| `uint` 이름에 `${ro.build.type}` | 버림. type 검사 log에 확장한 값 `userdebug`가 찍힌다 | 같음 |
| `uint` 이름에 `${ro.build.version.sdk}` | `37` 저장 | 같음 |
| 같은 파일의 앞선 entry를 가리키는 `${debug.rsp.plain}` | 버림(`property 'debug.rsp.plain' doesn't exist`) | 앞선 entry의 값으로 저장 |

마지막 행이 남은 차이이며 의도한 것이다(b). init은 이름을 property area에서
찾는다. `PropertyLoadBootDefaults`는 모든 파일을 map에 모은 뒤에 area에
적용하므로(`:1234-1239`), 확장 시점의 area에는 같은 load의 entry가 없고
kernel에서 파생한 `ro.boot.*` 등만 있다. 이 loader는 호출자가 넘긴 map에서
찾는다. map에는 같은 호출과 그 `import`가 먼저 저장한 entry, 그리고 호출
전에 들어 있던 값이 있다. 이 service는 빈 map으로 시작하고 build.prop를 읽는
시점의 area도 비어 있다. init과 같은 조회 대상을 쓰면 default 없는 참조는
모두 버리게 되므로, `import` 경로 확장이 이미 쓰던 map 조회를 값에도 그대로
적용했다. 포기한 것은 init과의 일치다. 같은 load의 앞선 entry를 참조하는
파일은 init에서는 그 entry가 빠지고 이 service에서는 저장된다. area에만 있는
property를 참조하려면 호출자가 load 전에 map에 넣어야 한다.

`load_properties_from_file`의 동작이 바뀌므로 0.6.0 대비 breaking change다.
값에 든 `$`는 `$$`로 써야 하고, 그렇지 않은 entry는 확장한 값으로 저장하거나
버린다.

검증: `properties_service.rs`
`build_prop_entry_with_mismatched_type_is_skipped`(같은 이름의 두 순서,
`import`한 파일, 확장한 값의 type 검사 포함),
`build_prop_version_that_selects_v1_is_refused`,
`build_property_parser.rs`
`test_accept_is_called_per_entry_before_the_map_changes`,
`test_values_are_expanded_before_the_checks`(위 표의 `local.prop` 줄),
`test_expand_props_aosp_parity`,
`test_expand_props_result_is_bounded`,
`test_chained_expansion_stops_at_the_line_bound`,
`test_expanded_values_are_bounded_per_pass`,
`property_info_parser.rs` `property_type_in_trie_reads_unpublished_bytes`.

### S3. `ctl.*` 저장 — 저장 동작은 (c), dispatch 없음은 (a), 종료 뒤 `ctl.stop` 응답은 `stop_handler_calls`로 수정

init은 `ctl.*`를 area에 저장하지 않는다. 권한을 검사한 뒤 control
message로 처리한다(`:505-512`, `:548-549`).

**변경 전(main).** build.prop loader는 `ctl.*`, `sys.powerctl`,
`selinux.restorecon_recursive`를 버린다(`build_property_parser.rs:424`).
하지만 runtime set 경로는 `ctl.start` 같은 이름을 그대로 area에 기록한다.

init은 control message를 queue에 넣고 main thread의 `HandleControlMessage`가
처리한 뒤 응답한다. 성공하면 `PROP_SUCCESS`, service가 없거나 action이
실패하면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`다(`init.cpp:556-649`).

**변경.** owner 결정에 따라 handler callback을 추가했다.
`ServiceOptions::control_handler(|msg: ControlMessage, reply: ControlReply|)`이며,
`ControlMessage`는 `action`(`ctl.` 뒤), `target`(값)과 요청을 보낸 process의
`pid`, `uid`, `gid`(SO_PEERCRED)를 담는다. `ControlReply`는 그 요청을 보낸
client의 connection fd를 소유하고, `send(self, handled: bool)`이 응답 code를 그
fd에 쓴 뒤 fd를 닫는다.
- 응답 주체와 시점. init의 `SendControlMessage`는 client socket의 fd를
  `socket->Release()`로 꺼내 control message와 함께 queue에 넣고
  (`property_service.cpp:451-457`), main thread의 `HandleControlMessages`가
  message를 처리한 뒤 그 fd에 결과를 `send`하고 `close`한다
  (`init/init.cpp:625-649`). 이 service도 fd를 요청과 함께 넘긴다. connection
  task(`handle_control`)는 이름을 검사한 뒤(`INVALID_NAME`) tokio `UnixStream`에서
  fd를 꺼내 `ControlReply`에 넣고, 요청을 queue에 넣은 다음 끝난다.
  `reply.send(true)`는 `PROP_SUCCESS`를, `reply.send(false)`는
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 보낸다. `send`는
  `MSG_NOSIGNAL | MSG_DONTWAIT`로 4 byte를 쓰는 동기 함수이고 tokio runtime을
  쓰지 않는다. 따라서 host는 handler 호출 안에서도, 호출이 반환한 뒤 host의
  다른 thread나 task에서도 `send`를 호출할 수 있다. client가 socket을 닫아서
  쓰기가 실패하면 debug log만 남긴다. init도 `send` 결과를 확인하지 않는다.
  요청은 properties service로 넘어가지 않으므로 area에 저장되지 않는다.
- 응답 없이 drop된 `ControlReply`. `Drop`이
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 보낸다. stop flag(아래
  `stop_handler_calls`)가 설정된 뒤이고 action이 `stop`이면 `PROP_SUCCESS`를
  보낸다. handler가 panic하면 인자로 받은 `ControlReply`가 unwind 중에
  drop되므로 client는 실패 응답을 받는다. host의 처리 task가 요청을 남긴 채
  끝나는 경우에도 channel에 남은 `ControlReply`가 drop되면서 같은 응답이
  전송된다. 응답을 받지 못한 채 기다리는 client가 생기는 경우는 host가
  `ControlReply`를 보유한 채 `send`도 drop도 하지 않을 때뿐이다.
- handler가 없으면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`로 거부한다.
- 호출 순서. task 하나(`handler_worker`)가 queue에서 요청을 하나씩 꺼내
  handler를 tokio blocking pool에서 호출한다. handler 호출을 한 번에 하나만
  실행하고, 그 호출이 반환한 뒤에 다음 요청을 꺼낸다. `ctl.*` 요청은 읽은
  순서대로 전달하고, 기록된 `sys.powerctl` command가 있으면 다음 `ctl.*`
  요청보다 먼저 전달한다(S10). init의 main loop는 iteration의 첫 단계에서
  `CheckShutdown`이 반환한 command로 `HandlePowerctlMessage`를 실행하고, 마지막
  단계에서 `HandleControlMessages`가 control message 하나를 처리한 뒤 fd로
  응답한다(`init/init.cpp:625-649`, `:1336-1338`). 반환이 늦은 handler 호출은
  그 뒤에 queue에 들어간 모든 `ctl.*` 요청의 전달을 그 시간만큼 늦춘다. init도
  control function을 main thread에서 실행하므로 같은 지연이 생긴다.
- 처리 순서. service가 고정하는 것은 handler 호출의 순서뿐이다. handler 호출
  안에서 `send`하는 host는 init처럼 요청을 읽은 순서대로 하나씩 처리하고
  응답한다. `(msg, reply)`를 channel로 넘기고 바로 반환하는 host는 처리 순서와
  응답 순서를 직접 정한다. 그 channel을 task 하나가 읽으면 호출 순서가
  처리 순서로 유지된다.
- 미응답 요청 한도. 요청은 읽힌 뒤 `ControlReply`가 `send`되거나 drop될
  때까지 미응답으로 계산된다. queue에서 대기 중인 요청, handler 호출 중인
  요청, host가 `ControlReply`를 보유한 요청이 모두 포함된다. 새 요청을 읽은
  시점에 미응답 요청이 102개(`MAX_UNANSWERED_CONTROL_MESSAGES`)이면 그 요청은
  handler 호출 없이 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 받는다. init의
  `QueueControlMessage`(`init/init.cpp:613-623`)는
  `pending_control_messages.size() > 100`이면 message를 버리고,
  `SendControlMessage`(`property_service.cpp:458-463`)가 넘겨받은 fd로 같은
  code를 보낸다. 거부 조건은 queue 길이뿐이며, main thread가 control
  message를 처리 중인지와는 무관하다. 따라서 init의 queue에는 101개까지
  들어가고, main thread가 처리 중인 message 하나는 queue에서 빠진 상태이므로
  응답을 받지 않은 message는 최대 102개다. handler 호출 안에서 응답하는
  host에서는 control handler 호출이 진행 중일 때 거부 조건이 init과 같다.
  worker가 powerctl handler를 호출하는 중이어서 control handler 호출이 없을
  때는 다르다. init은 queue에 101개가 있으면 다음 message를
  거부하지만, 이 service는 미응답이 101개이므로 하나를 더 받는다. `ControlReply`를 보유하는 host에서는 보유분도 한도에 포함되므로,
  service process가 미응답 `ctl.*` 요청 때문에 열어 두는 fd는 102개를 넘지
  않는다.
- 응답 대기 시간. service는 응답까지의 시간을 제한하지 않는다. 요청이 queue에
  들어간 뒤에는 connection task가 끝난 상태이므로 `CLIENT_TIMEOUT`(10 s)이
  적용되지 않고, 그 요청은 connection permit(`MAX_CONCURRENT_CLIENTS` = 64)도
  차지하지 않는다. bionic client와 Android의 `rsproperties` client는 응답이 올
  때까지 기다린다. 그 밖의 target에서 `rsproperties` client는 2 s 뒤
  `TimedOut`을 반환한다(`system_property_set.rs` `RESPONSE_TIMEOUT`). client가
  socket을 닫은 요청도 handler에 전달되고, 그 요청의 응답만 버려진다. init도
  client가 socket을 닫은 control message를 처리한다.
- blocking thread 사용량. handler 호출이 차지하는 blocking thread는 queue
  길이와 무관하게 최대 하나다. persist 파일 쓰기도 같은 pool에서 실행되지만,
  반환하지 않는 handler가 그 쓰기를 막는 경우는 host runtime의
  `max_blocking_threads`가 1일 때뿐이다.
- init이 하는 권한 검사(`CheckControlPropertyPerms`,
  `property_service.cpp:468-495`)는 하지 않는다((a)). init은 요청을 보낸
  process의 SELinux context가 대상의 `ctl.*` 이름을 set할 수 있는지 검사하고,
  실패하면 queue에 넣기 전에 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 보낸다
  (`:505-512`). 이 service는 socket에 연결할 수 있는 모든 process의 요청을
  handler에 전달한다. handler가 `uid`, `gid`, `pid`로 판단해
  `reply.send(false)`를 호출하면 client는 init의 권한 거부와 같은 code를
  받는다. init에서는 거부된 요청이 queue에 들어가지 않지만, 이 service에서는
  그 요청도 응답 전까지 미응답 한도에 포함된다. SELinux context를 기준으로 한
  판정은 구현할 수 없다.

handler가 요청별로 무엇을 실행하고 무엇을 반환해야 init과 같은 결과가 되는지는
`rsproperties-service/README.md`의 "Answering as init does" 절에 표로 정리했다
(`ctl.stop`의 `init.svc.<name>` 전이, `sys.powerctl` command 판정).

**service 종료 뒤의 `ctl.*`.** init에는 control message 수신을 끊는 경로가
있다. `HandlePowerctlMessage`가 `StopSendingMessages()`를 호출하면
(`init/reboot.cpp:1173-1175`) `accept_messages`가 `false`가 되고
(`property_service.cpp:141-144`), 그 뒤 `SendControlMessage`는 message를
queue에 넣지 않는다(`property_service.cpp:439-449`).
- (a) 종료 뒤 수신 차단. `SocketService`의 `Drop`이 handler queue를 닫는다. 그
  뒤에 읽은 `ctl.*` 요청은 handler를 호출하지 않고
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 받는다. 닫는 시점에 queue에 남아 있던
  `ctl.*` 요청도 handler 호출 없이 같은 응답을 받는다. 이 응답은 queue에 있던
  `ControlReply`가 drop되면서 전송된다.
- (a) handler 호출 대기. `ServiceHandle::shutdown`과 `join`은 두 actor의 task가
  끝난 뒤 `handler_worker`가 끝날 때까지 기다린다. `handler_worker`는 queue가
  닫히면 실행 중인 호출을 마치고, 닫히기 전에 기록된 `sys.powerctl` command가
  있으면 그 호출도 마친 뒤(client가 이미 `PROP_SUCCESS`를 받았기 때문이다) 두
  handler를 drop하고 끝난다. 반환 시점에는 실행 중인 handler 호출이 없고 그
  뒤에 시작하는 호출도 없으며, service가 가진 handler clone도 없다. timeout은
  없다. 반환하지 않는 handler 호출이 있으면 `shutdown`과 `join`도 반환하지
  않는다. 이때 socket 파일 unlink와 lock 해제는 이미 끝난 상태다. init도
  control function을 main thread에서 실행하고(`init/init.cpp:625-649`, main loop
  `:1337`) timeout을 두지 않는다. 시작에 실패한 `run`도 같은 대기를 거친 뒤
  `Err`을 반환한다. handle을 drop하는 경로는 기다리지 않는다.
- (a) host가 보유한 `ControlReply`. `shutdown`, `join`, 실패한 `run`은 host가
  보유한 `ControlReply`를 기다리지 않는다. `handler_worker`는 handler 호출이
  반환한 시점에 그 호출의 `ControlReply`를 갖고 있지 않으므로, 대기 대상은
  handler 호출까지다. `ControlReply`는 client의 fd를 직접 소유하고 `send`가
  runtime을 쓰지 않으므로, `shutdown`이 반환한 뒤나 runtime이 종료된 뒤에
  호출한 `send`도 client에 도달한다. init에는 property service만 종료하는
  경로가 없으므로 비교할 동작이 없다.

**`ServiceHandle::stop_handler_calls`.** init은 `accept_messages`가 `false`일 때
`ctl.stop`에 `PROP_SUCCESS`를 반환하고(`property_service.cpp:445`), 나머지
`ctl.*`에 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 반환한다. `ctl.stop` 예외는
commit `c9d44b84f`(2024-04-30, "Don't reject "ctl.stop.*" during shutdown.",
issuetracker 336223505)에서 들어갔다. 비교는 `msg == "stop"`이므로
`ctl.interface_stop`, `ctl.sigstop_off` 같은 action은 예외에 들지 않는다.

변경 전의 이 service에는 `accept_messages`에 해당하는 상태가 없었다. handler
호출이 끊기는 시점은 `SocketService`의 `Drop`뿐이고, 그 시점에는 socket 파일이
unlink되므로 새 연결은 connect에서 실패한다. 따라서 init의 응답을 종료 뒤에만
적용하면, 종료 전에 accept된 연결만 그 응답을 받는다. owner 결정에 따라
service가 실행 중인 상태에서 handler 호출만 끊는 method를 추가했다.

`stop_handler_calls(&self)`는 `Handlers`가 가진 `HandlerCallsStop`(`AtomicBool`)을
설정한다. powerctl handler가 `PowerctlOutcome::ShuttingDown`을 반환하면
`handler_worker`가 같은 flag를 설정한다(S10). 따라서 `sys.powerctl`로 시작하는
shutdown에서는 host가 `stop_handler_calls`를 호출할 필요가 없고, 이 method는
`sys.powerctl` set 없이 host가 종료 절차를 시작하는 경로에서 쓴다. flag가
설정된 뒤에 읽은 요청은 다음 응답을 받는다.
- `ctl.stop`: handler를 호출하지 않고 `PROP_SUCCESS`. control handler가 없어도
  같다.
- 그 밖의 `ctl.*`: `PROP_ERROR_HANDLE_CONTROL_MESSAGE`.
- 이름 검사(`PROP_ERROR_INVALID_NAME`)는 flag 검사보다 먼저 한다. init도
  `CheckPermissions`의 `IsLegalPropertyName`이 `SendControlMessage`보다 먼저다.
- `sys.powerctl`: properties service의 검사는 그대로 하고, 통과하면 handler를
  호출하지 않고 `PROP_SUCCESS`(S10).
- 그 밖의 이름: 변경 전과 같이 처리한다. init도 `accept_messages`가 `false`인
  동안 area에 기록한다.

flag 검사는 queue가 닫혔는지 확인하는 검사보다 먼저 한다. flag를 설정한 뒤
`shutdown`하면, 종료 전에 accept된 연결이 종료 뒤에 보낸 `ctl.*` 요청도 위
응답을 받는다. `sys.powerctl`은 properties service의 응답을 받은 뒤에 flag를
확인하므로, properties service가 끝난 뒤에 읽은 요청은 검사를 받지 못하고
`PROP_ERROR_SET_FAILED`를 받는다. 그 밖의 이름도 같다. flag를 설정하지 않고 `shutdown`하면 위 "(a) 종료 뒤 수신 차단"의 거부
응답이 유지된다.

init과 남는 차이는 다음과 같다.
- (a) 판정 주체. init은 `HandlePowerctlMessage`가 command를 유효하다고 판정한
  뒤 `StopSendingMessages()`를 직접 호출한다. 이 service는 `sys.powerctl`
  command를 해석하지 않으므로(S10), 그 판정을 powerctl handler의 반환값
  (`ShuttingDown`)으로 받는다. `ctl.stop`의 `PROP_SUCCESS`는 대상을 멈추지 않고
  보내는 응답이다. init에서는 shutdown이 모든 service를 종료하므로 이 응답이
  결과와 어긋나지 않는다. 이 service에서는 host가 대상 process를 종료하는
  절차에 들어간 경우에만 같은 조건이 성립하므로, handler는 그 절차를 시작한
  경우에만 `ShuttingDown`을 반환해야 한다.
- (b) flag 없이 종료할 때의 거부. flag를 설정하지 않은 채 `shutdown`하거나
  handle을 drop하면 모든 `ctl.*`가 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 받는다.
  service process의 종료는 기기 shutdown이 아닐 수 있고, 그때는 `ctl.stop`의
  대상이 멈춘다고 볼 근거가 없다.
- (b) flag 설정 시점에 queue에 남은 요청. init의 main loop는
  `if (!IsShuttingDown()) HandleControlMessages();`이므로(`init/init.cpp:1336-1338`),
  `EnterShutdown`(`init/reboot.cpp:1029-1031`) 뒤에는 queue에 남은 control
  message를 처리하지 않고 응답도 보내지 않는다. 그 client는 응답을 받지
  못한다. 이 service의 `handler_worker`는 호출 직전에 flag를 읽고, 설정되어
  있으면 handler를 호출하지 않은 채 flag 설정 뒤에 읽은 요청과 같은 응답
  (`ctl.stop`은 `PROP_SUCCESS`, 그 밖은 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`)을
  보낸다. handler를 호출하지 않는 점은 init과 같고, 응답을 보내는 점이 다르다.
  기록되어 있던 `sys.powerctl` command는 버린다. init의 `CheckShutdown`도
  `IsShuttingDown()`이면 command를 반환하지 않는다(`init/init.cpp:262-269`).
  flag 설정 시점에 host가 보유한 `ControlReply`는 host가 응답한다. host가
  `send`를 호출하면 인자로 준 값이 그대로 전송되고, flag 설정 뒤에 drop하면 위와
  같은 응답이 전송된다. 따라서 종료 절차에 들어간 host는 처리하지 않은 요청의
  `ControlReply`를 drop하면 된다.
- (a) 처리 순서의 결정 주체. init은 main thread가 control function을 실행한 뒤
  응답하므로, 처리 순서와 응답 순서가 읽은 순서와 같다. 이 service가 고정하는
  것은 handler 호출 순서다. handler 호출 안에서 응답하는 host는 init과 같은
  순서를 얻는다. `ControlReply`를 넘겨서 여러 task가 처리하는 host에서는 처리
  순서와 응답 순서가 host의 구현에 따라 정해진다.
- (a) flag를 읽은 뒤의 호출. `handler_worker`가 flag를 읽은 뒤 `spawn_blocking`을
  호출하기 전에 `stop_handler_calls`가 flag를 설정하면, 그 호출 하나는
  `stop_handler_calls`가 반환한 뒤에 시작된다. `shutdown`과 `join`이 그 호출을
  기다린다. `ShuttingDown` 반환 경로에서는 flag를 설정하는 주체가
  `handler_worker` 자신이므로 이 간격이 없다. init은 `accept_messages_lock`을
  잡고 검사와 queue 삽입을 함께 하고 main thread 하나가 처리하므로 이 간격이
  없다.
- (a) 초기 상태. init의 `accept_messages`는 `StartSendingMessages()`
  (`property_service.cpp:137-140`) 전에도 `false`다. 이 service의 flag는 `run`
  시점에 해제 상태이고, 되돌리는 method는 없다.

**포기한 것.** flag를 되돌리는 method는 두지 않았다. init에도
`StopSendingMessages` 뒤에 `StartSendingMessages`를 다시 호출하는 경로가 없다
(shutdown은 reboot로 끝난다). shutdown을 취소할 수 있는 host는 service를 다시
`run`해야 한다.

handler 호출의 동시 실행도 포기했다. 대상이 서로 다른 `ctl.*` 요청도 앞 호출이
반환할 때까지 기다린다. 호출을 동시에 실행하면 서로 다른 client가 보낸 요청과,
client가 `TimedOut` 뒤에 다시 보낸 요청이 읽은 순서와 다른 순서로 handler에
도착할 수 있다. 한 client가 응답을 받은 뒤에 보낸 다음 요청은 동시 실행에서도
순서가 유지된다. 요청을 동시에 처리하려는 host는 handler에서 `(msg, reply)`를
넘기고 바로 반환하면 된다. 종료 시점에 queue에 남은 `ctl.*` 요청은 실행하지
않고 거부한다. 남은 요청을 모두 실행한 뒤 종료하면 `shutdown`의 대기 시간이
queue 길이(최대 102)에 비례한다.

응답 대기 timeout을 두지 않았다. host가 `ControlReply`를 보유한 채 `send`도
drop도 하지 않으면 bionic client는 계속 기다리고, 그 요청은 미응답 한도에 계속
포함된다. 102개가 모두 그 상태가 되면 이후의 모든 `ctl.*` 요청이 거부된다.
init도 control function이 반환하지 않으면 main thread가 멈춘다. service가
timeout으로 실패를 응답하면, host가 이미 실행한 action에 실패 응답이 전송되는
경우가 생긴다.

`shutdown`이 host 보유 `ControlReply`를 기다리는 방식도 채택하지 않았다.
`ServiceHandle`을 소유한 task가 `ControlReply`도 처리하는 host(doc 예제의
구조)에서는 `shutdown`을 기다리는 동안 그 task가 `send`를 호출할 수 없으므로,
기다리게 하면 `shutdown`이 반환하지 않는다. 그 결과 `shutdown`이 반환한
시점에도 응답을 기다리는 client가 남아 있을 수 있다.

handler가 `bool`을 반환하는 형태(`Fn(&ControlMessage) -> bool`)는 제거했다.
이 형태는 0.6.0에 없었고 이 branch에서만 존재했다. 반환값으로 응답하면 응답
시점이 handler 호출의 반환 시점으로 고정되므로, host가 다른 task에서 처리한
결과를 응답하려면 handler 호출 안에서 그 결과를 기다려야 하고 그동안 뒤의
요청이 전달되지 않는다.

**test.** `control_reply.rs` `send_answers_and_closes_and_uncounts`,
`dropped_reply_answers_by_the_stop_flag`, `send_needs_no_runtime`,
`probe_reports_the_answer_or_the_drop`,
`answer_to_closed_client_is_discarded_without_sigpipe`,
`socket_service.rs` `control_messages_go_to_the_handler`,
`control_calls_are_serial_and_the_queue_is_bounded`,
`reply_is_sent_when_the_host_answers`,
`reply_dropped_by_the_host_answers_by_the_stop_flag`,
`replies_held_by_the_host_count_toward_the_bound`,
`queued_requests_are_answered_and_held_replies_usable_without_the_runtime`,
`shutting_down_outcome_stops_the_handler_calls`,
`close_ends_the_worker_after_its_calls`, `dropping_every_clone_ends_the_worker`,
`stopped_handler_calls_get_init_shutdown_replies`,
`lib.rs` `stop_handler_calls_answers_as_init_during_shutdown`,
`powerctl_shutting_down_outcome_stops_handler_calls`,
`ctl_request_read_after_shutdown_is_refused`,
`unanswered_control_requests_hold_no_connection_slot`,
`reply_held_by_the_host_outlives_shutdown`.

### S4. 실패 응답 코드 — (c)

**변경 전(main).** service는 실패하면 항상 `PROP_ERROR(-1)`를 보낸다. init은
`PROP_ERROR_READ_ONLY_PROPERTY`, `INVALID_NAME`, `INVALID_VALUE`,
`PERMISSION_DENIED`, `SET_FAILED`, `HANDLE_CONTROL_MESSAGE` 등을 구분한다.
client의 `Error::ServiceError { code }`에는 Linux에서 항상 -1만 들어온다.

**변경.** `wire::PROP_ERROR_*`에 bionic의 code 값을 추가하고, 아래 "init과
다른 점"을 제외하면 init과 같은 조건에서 같은 code를 보낸다.
- command word를 읽지 못함: `READ_CMD`(`:614-617`). 연결만 하고 닫은 peer에게도
  보내며, 이미 닫힌 peer에는 전달되지 않는다.
- 읽을 수 없는 field와 한도를 넘는 길이: `READ_DATA`(init `RecvString`, 한도는
  아래 "init과 다른 점" 참고).
- 알 수 없는 command: `INVALID_CMD`.
- 이름 검사 실패, 또는 이름에 NUL이 있거나 UTF-8이 아님: `INVALID_NAME`.
- type 불일치, 값이 너무 길거나 UTF-8이 아님: `INVALID_VALUE`.
- 값이 이미 있는 `ro.`: `READ_ONLY_PROPERTY`.
- 그 밖의 저장 실패: `SET_FAILED`.

검사 순서는 init(`RecvString` 두 번 → `CheckPermissions` → `PropertySet`)과
같다. 두 field를 모두 읽은 뒤 이름, `ctl.`, type, 값 길이, 저장 순으로 검사한다.
V2 field의 NUL·UTF-8 오류는 읽을 때 발견하지만 두 field를 다 읽을 때까지
보류한다. 그래서 두 번째 field를 읽지 못하면 `READ_DATA`, 이름과 값이 둘 다
잘못되면(예: 이름 `.bad`, 값에 NUL) 이름이 먼저 걸려 `INVALID_NAME`이다
(`property_service.cpp:500-502`). 공개 `PROP_ERROR`(-1) 상수는 그대로 두었다.

client는 응답 code를 `Error::SetRejected { name, reason: SetRejection }`으로
반환한다(변경 전 이름은 `Error::ServiceError { name, code: i32 }`).
- bionic `__system_property_set`은 code를 호출자에게 넘기지 않는다. log에
  `__prop_error_to_string(result)`와 code를 남기고 -1을 반환한다
  (`system_property_set.cpp:258-327`). libbase `SetProperty`는 `bool`이다. code를
  호출자에게 넘기는 것은 이 crate가 추가한 동작이다(b).
- `SetRejection`은 `__prop_error_to_string`이 구분하는 code 9개에 variant를
  하나씩 둔 `#[non_exhaustive]` enum이다. `PROP_ERROR`(-1)는 `Unspecified`,
  그 밖의 값은 `Unknown(i32)`이고, `code()`가 원래 숫자를 반환한다. `i32`를
  그대로 두면 호출자가 `wire::PROP_ERROR_*` 상수와 직접 비교해야 하고,
  `Debug`·`Display` 출력에 숫자만 나온다. `Unknown(n)`의 `n`이 나중 version에서
  variant를 얻으면 `Unknown(n)`을 match하던 코드는 그 arm에 도달하지 않는다.
  그래서 특정 숫자를 비교할 때는 `code()`를 쓰라고 문서에 적었다.
- 이름을 바꾼 이유는 `rsproperties_service::ServiceError`(service 종료 원인)와
  이름이 같았기 때문이다. 두 타입은 관계가 없다.

service는 writer의 오류 중 `Error::ReadOnlyProperty`만 `READ_ONLY_PROPERTY`로
바꾼다. 변경 전에는 `ro.` 거부가 `Error::PermissionDenied`였고, 이 variant는
파일 mode 거부와 read-only mapping에 대한 쓰기에도 쓰였다. service는 variant
전체를 `READ_ONLY_PROPERTY`로 바꿨으므로, 뒤의 두 조건이 `set` 중에 발생하면
client는 `ro.` 거부로 보고받는다. 현재 코드에서 service의 writer는 두 조건에
도달하지 않는다. writer는 시작할 때 모든 area를 `PropertyAreaMap::new_rw`로
열고, 파일 mode 검사는 `new_ro`와 `property_info` load에만 있다. 따라서 이
변경으로 service의 응답이 달라지는 경우는 없고, 변환 대상을 `ro.` 거부로
한정한 것이다. 두 조건이 `set` 경로에 생기면 응답은 `SET_FAILED`다.

init과 다른 점은 다음과 같다.
- `sys.powerctl`=`reboot,userspace`의 응답은 수정했다(c). init
  `HandlePropertySet`은 이 값에 `PROP_ERROR_INVALID_VALUE`를 보낸다
  (`:565-567`, "Userspace reboot is deprecated."). 위치는 `CheckPermissions`
  뒤, `PropertySet` 앞이다. 이 service는 값을 저장하고 `PROP_SUCCESS`를
  보냈다. `apply_set`이 type 검사 뒤에 같은 값을 `INVALID_VALUE`로 거부하도록
  고쳤다. 기준은 `android-17.0.0_r1`이다. 이전 release의 init은
  `is_userspace_reboot_supported()` 결과에 따라 이 값을 처리했다(release별
  조건은 확인 필요).
- `sys.powerctl`의 나머지 값은 area에 저장하지 않고 powerctl handler로
  넘긴다. 내용은 S10에 있다.
- (a) `selinux.restorecon_recursive`는 일반 property처럼 저장한다. pid가 1이
  아닌 client가 빈 값이 아닌 값을 보내면 init은 restorecon thread에 경로를
  넘기고 저장 없이 `PROP_SUCCESS`를 보낸다(`:575-579`). 그 thread는
  `selinux_android_restorecon`이 끝나면, 실패했을 때도,
  `SetProperty(kRestoreconProperty, path)`로 경로를 저장한다(`:207-211`).
  따라서 restorecon이 끝난 뒤의 값은 이 service가 저장하는 값과 같고, 차이는
  저장 시점뿐이다. 이 service에는 restorecon을 수행할 대상이 없다. 이 문서의
  이전 판은 init이 이 값을 저장하지 않는다고 적었는데, thread의 `SetProperty`를
  빠뜨린 서술이었다.
- 위 세 항목의 init 동작은 API 37 emulator(userdebug, `adb root`)에서
  실측했다. `setprop sys.powerctl reboot,userspace`는 실패했고 libc log에
  `PROP_ERROR_INVALID_VALUE (0x14)`가 남았다. `setprop sys.powerctl rsp_bogus`는
  성공했고 `getprop sys.powerctl`은 빈 값이었으며, init log에 `powerctl:
  unrecognized command 'rsp_bogus'`가 남았다. `setprop
  selinux.restorecon_recursive /data/local/tmp/rsp_rc` 2초 뒤의 `getprop`은 그
  경로였고, 빈 값을 set한 뒤에는 빈 값이었다. build.prop loader는 두 이름을
  버린다(`build_property_parser.rs`의 `ctl.*` 제외 분기). 검증:
  `properties_service.rs` `apply_set_returns_init_codes`(`sys.powerctl`이 area에
  없다는 assert 포함).
- (c) V2 값에 NUL이 있으면 `INVALID_VALUE`로 거부한다. init
  `IsLegalPropertyValue`(`init/util.cpp:406-416`)는 `mbstowcs(nullptr,
  value.data(), 0)`로 UTF-8을 검사하는데, 이 호출은 첫 NUL에서 멈추므로 NUL을
  거부하지 않는다. `debug.x`=`"a\0b"`에 init은 `PROP_SUCCESS`, 이 service는
  `INVALID_VALUE`를 보낸다. `ctl.*` 값은 init에서 `PropertySet`을 거치지 않고
  control message로 넘어가므로(`property_service.cpp:548-549`), NUL이나 UTF-8이
  아닌 byte가 있어도 거부되지 않는다. 이 service는 둘 다 `INVALID_VALUE`로
  거부한다.
- (c) 길이 한도가 다르다. init `RecvString`은 `0xffff`보다 긴 field만
  거부한다(`:249`). 이 service는 `MAX_WIRE_NAME_LEN`(1024)보다 긴 이름과
  `MAX_WIRE_VALUE_LEN`(8192)보다 긴 값에 `READ_DATA`를 보낸다. 1025–65535 byte
  이름과 8193–65535 byte 값을 init은 읽은 뒤 이후 검사로 판정한다. 예를 들어
  10000 byte 값은 `ro.` 이름이면 `IsLegalPropertyValue`의 길이 검사를
  통과하고, 그 밖의 이름이면 `INVALID_VALUE`를 받는다. 이 service는 두 경우
  모두 `READ_DATA`를 보낸다.

### S5. `ro.property_service.version` — (c), 변경 후에는 env var로 V1을 고른 이 crate의 client도 V2를 쓴다

**변경 전(main).** init은 `ro.property_service.version=2`를 설정한다(`:1552`).
service는 설정하지 않는다. 이 crate의 client는 값이 없으면
`PROPERTY_SERVICE_VERSION`을 보고, 그 변수도 없으면 V2를 쓴다. 영향을 받는
것은 bionic 규칙(값이 없으면 V1)을 따르는 다른 client다.

**변경.** build.prop entry를 적용한 뒤 `ro.property_service.version=2`를
설정한다. build.prop가 이미 값을 정했다면 그 값이 남는다(`ro.`는 write-once).
이 crate의 client(`system_property_set.rs` `protocol_version`)는 global
reader가 초기화되어 있으면 이 property를 env var보다 먼저 읽는다. v0.6.0
client도 순서가 같다. 그래서 `PROPERTY_SERVICE_VERSION=1`로 V1을 고르던
client는 global reader가 먼저 초기화된 process에서 V2 frame을 보내고 4-byte
응답을 기다린다. service가 V1을 처리하지 않으므로(S9) build.prop의
`ro.property_service.version=1`로 V1을 유지하는 방법은 없다. 그 entry가 있으면
`run`이 실패한다.
S1의 persist load 뒤에는 `ro.persistent_properties.ready=true`를 설정한다.
persist 파일을 지정하지 않아도 설정한다. init이 항상 설정하기 때문이다.

### S6. 변경 알림 hook — (a)

init은 property가 바뀌면 `PropertyChanged`로 `on property:` trigger를
실행한다(`:183`). service를 내장한 app이 set에 반응할 callback은 없다.
init.rc trigger는 emulation 범위 밖이다. 따라서 parity 결함이 아니라 기능
확장 후보로 분류한다.

### S7. properties dir writer lock — (a), 해제 방식은 (b)

**AOSP.** writer는 init 하나다. init은 `/dev/__properties__`를 만들고
`property_info`를 쓴 뒤 `__system_property_area_init`을 호출한다
(`property_service.cpp:1410-1412`). bionic은 area 파일을
`O_CREAT | O_EXCL`로 연다(`prop_area.cpp:60`). `/dev`는 부팅마다 새로 만드는
tmpfs이므로 이전 실행의 파일이 남지 않고, 같은 directory에서 area를 두 번
만들면 `EEXIST`로 실패한다. lock 파일은 없다.

**rsproperties.** Linux에서는 properties dir이 실행 사이에 남고, 같은
directory를 인자로 받은 process가 둘 이상일 수 있다. writer는 area 파일을
temp 파일에 쓴 뒤 rename으로 교체하므로 `O_EXCL`로는 두 번째 writer를
거부할 수 없다. 그래서 `<dir>/.writer_lock`에 `flock(LOCK_EX|LOCK_NB)`을
잡는다(`AreaWriterLock::acquire`). `property_info` 게시
(`publish_property_info`)와 area 생성(`new_area_locked`)은 이 lock을 쥔 채로
실행한다. 두 번째 writer는 `property_info`와 area 파일을 바꾸기 전에
`Error::Lock`으로 실패한다. 여기까지가 (a)다.

**해제 방식 — (b).**
- lock을 쥔 값(`AreaWriterLock` 또는 그것을 넘겨받은 `SystemProperties`)이
  drop될 때 `flock(LOCK_UN)`을 호출한다. 0.6.0은 fd를 닫기만 했다. `flock`은
  같은 open file description을 가리키는 fd가 모두 닫혀야 풀리는데,
  `std::process::Command`가 만든 child는 `exec` 전까지 fd 사본을 가진다.
  그래서 drop 직후의 `acquire`가 실패할 수 있었다. thread 4개가 `/bin/true`를
  반복 실행하는 동안 drop 후 `acquire`를 20000회 반복하면, 수정 전에는
  1696회와 1649회 실패했고 수정 후에는 실패하지 않았다.
- `acquire`를 호출한 pid와 drop 시점의 pid가 다르면 `LOCK_UN`을 호출하지
  않는다. `fork`만 하고 `exec`하지 않은 child가 값을 drop해도 parent의 lock은
  유지된다. child의 fd 사본은 child가 종료하거나 fd를 닫을 때 없어진다.
- `LOCK_UN`이 실패하면 `warn!`을 남긴다. 이 경우 lock은 fd 사본이 모두 닫힐 때
  풀린다.

### S8. prop 파일을 읽지 못했을 때의 반환값 — open 실패로 시작 실패는 (b)

**AOSP.** init의 `load_properties_from_file`은 `Result<void>`를 반환한다
(`property_service.cpp:823-836`). `ReadFile`이 실패하면 원인을 구분하지 않고
`Error()`를 반환한다. 원인은 파일 없음, 마지막 path component가 symlink인
경우(`O_NOFOLLOW`), group 또는 other write bit가 있는 경우
(`util.cpp:179-180`, "Skipping insecure file")다. 호출부는 세 곳에서만 log를
남긴다: second stage ramdisk prop(`:844`, `WARNING`), recovery의
`/prop.default`(`:1155`, `ERROR`), `/system/build.prop`(`:1207`, `WARNING`).
partition별 파일은 반환값을 읽지 않거나, `.ok()`로 legacy 경로를 시도할지만
정한다(`:1167`, `:1213-1220`). 어느 호출부도 부팅을 중단하지 않는다.

**rsproperties.** `load_properties_from_file`은 두 경우를 다른 값으로
반환한다.

- open 또는 read 실패는 `Err`다. service는 `build_prop_files`의 파일 하나가
  `Err`이면 `property_info`를 게시하기 전에 시작을 중단한다
  (`StartupStage::PropertiesService`). 0.6.0도 같은 동작이다. init과 다른
  점이며 (b)로 분류한다. init의 경로 목록에는 기기 구성에 따라 없는 partition
  파일이 들어 있지만, `build_prop_files`는 호출자가 직접 지정한 경로다.
- group 또는 other write bit가 있는 파일은 읽지 않고
  `Ok(PropFileOutcome::SkippedWritable { mode })`를 반환한다. 이 branch
  이전에는 `Ok(())`였고, 읽은 경우와 구분할 수단은 `warn` log뿐이었다.
  umask 002로 checkout한 저장소의 파일은 mode가 0664여서 이 경우에 해당한다.
  이 crate의 test는 그래서 `tests/android/*.prop`의 0644 사본을 만들어
  load한다(`copy_fixture_0644`).
  service는 이 값을 받으면 `error` log를 남기고 그 파일 없이 시작한다. 계속
  진행한다는 점은 init과 같다.
- `import`한 파일의 실패(open 실패, write bit, depth 초과)는 반환값에 반영하지
  않고 `warn` log만 남긴다. init의 `LoadProperties`도 import 실패를
  `LOG(WARNING)`으로만 남긴다(`:764-767`).

**포기한 것.** write bit가 있는 파일을 `Err`로 바꾸면 init의 `Result<void>`와
형태가 같아진다. 그렇게 하지 않은 이유는 service 호출부가 `?`로 전파하므로
0664 파일 하나 때문에 service가 시작하지 못하게 되기 때문이다. 대신 enum에
`#[must_use]`를 붙여 반환값을 읽지 않는 호출부가 compile 경고를 받게 했다.

### S9. V1(`PROP_MSG_SETPROP`) 미지원 — (b), owner 지시

**AOSP.** `handle_property_set_fd`는 `PROP_MSG_SETPROP`(`:621`)와
`PROP_MSG_SETPROP2`(`:651`)를 모두 처리한다. V1 frame은 command word 뒤에
name `PROP_NAME_MAX`(32) byte와 value `PROP_VALUE_MAX`(92) byte가 고정 길이로
이어진다. init은 V1 set을 `HandlePropertySetNoSocket`으로 적용하고 응답 없이
connection을 닫는다. 그 밖의 command word에는 `PROP_ERROR_INVALID_CMD`를
보낸다(`:685-686`).

**변경 전(0.5.0–0.6.0).** service가 V1 frame을 읽어 set을 적용하고 응답 없이
connection을 닫았다.

**변경.** owner 지시에 따라 V1 처리를 제거했다. socket service
(`socket_service.rs` `handle_client`)는 command word가 `PROP_MSG_SETPROP`이면
name과 value를 읽지 않고 `PROP_ERROR_INVALID_CMD`를 보낸 뒤 connection을
닫는다. 아무것도 set하지 않고, peer pid를 넣은 `warn` log를 남긴다. 응답
코드는 init이 알 수 없는 command word에 보내는 코드와 같다. `SetProtocol`
enum과 `handle_setprop_v1`은 삭제했다.

- V1 client가 받는 결과. V1 client는 응답을 읽지 않는다. bionic은 socket이
  닫히기를 250 ms까지 기다린 뒤 0을 반환한다
  (`bionic/libc/bionic/system_property_set.cpp:210`). 이 crate의 client
  (`wait_for_socket_close`)도 수신한 byte를 버리고, `ECONNRESET`은 log에만
  남기고 `Ok`를 반환한다. 따라서 V1 client는 적용되지 않은 set에 성공을
  반환한다. 거부 사실이 남는 곳은 service의 `warn` log뿐이다. debug build의
  `example_service`에 `PROPERTY_SERVICE_VERSION=1`로 실행한 `setprop`
  example을 연결해 확인했다. `setprop`은 exit code 0으로 끝났고, service는
  거부 log를 남겼고, 그 이름은 area에 없었다.
- V1을 보내는 client. bionic은 `ro.property_service.version`을 `atoll`로 읽어
  2 이상이면 V2를 보낸다(`:246`). service가 이 값을 2로 설정하므로(S5)
  bionic 규칙을 따르는 client는 V1을 보내지 않는다. 이 crate의 client는 그
  process가 global reader를 만들기 전에 `set`을 호출했고
  `PROPERTY_SERVICE_VERSION`이 2 이상의 숫자가 아닐 때만 V1을 보낸다
  (`system_property_set.rs` `protocol_version`).
- (b) build.prop 값 검사. build.prop가 `ro.property_service.version`을 2 이상의
  숫자가 아닌 값으로 정하면 `run`이 `InvalidInput`으로 실패한다
  (`StartupStage::PropertiesService`, properties dir에 쓰기 전;
  `properties_service.rs` `init_system_properties_sync`). `ro.` 이름은
  write-once이므로 그 값이 area에 남고, 그 값을 읽은 client는 V1을 보내며,
  service는 그 frame을 거부하는데 client는 성공을 반환하기 때문이다. init에는
  이 검사가 없다. 판정식은 이 crate의 client와 같은
  `trim().parse::<u32>()`이다. `"2x"`는 bionic의 `atoll`로는 2이지만 이
  crate의 client는 V1로 판정한다. 이 검사는 load 중 type 검사(S2)를 통과해
  map에 남은 값에만 적용된다. 그래서 `run`이 실패하는 값은 type 검사를
  통과하고(이름에 type이 없으면 모든 값이 통과한다) 2부터 4294967295까지의
  10진수가 아닌 값이다. type이 없으면 `"1"`, `"v2"`, `"2x"`가 모두 여기에
  해당한다. `int` type(AOSP `plat_property_contexts`)에서는 `"1"`, `"0"`,
  빈 값, `"0x10"`, `"4294967296"`이 해당한다. `check_type`은 빈 값을 type과
  무관하게 통과시키고, `int`를 `0x` 16진수와 `i64` 범위로 판정하기 때문이다.
  type 검사가 거부하는 값(`int`에서 `"v2"`, `"2x"`)은 load 중 버려져 이 검사에
  도달하지 않고, service가 `2`를 설정한다(S2).

**포기한 것.** V1만 구현한 client(Android O 이전 libc와 같은 규칙)는 이
service에 set을 적용할 수 없다. V1 frame을 끝까지 읽은 뒤 닫는 방법도 있으나
그렇게 하지 않았다. 읽지 않은 124 byte가 수신 queue에 남은 채 닫으면 client의
다음 read가 `ECONNRESET`을 받을 수 있지만, V1 client는 그 error를 실패로
취급하지 않고 응답 4 byte는 그 전에 queue에 들어가 있다. build.prop 값 검사
대신 error log만 남기는 방법, build.prop보다 먼저 2를 set해 값을 고정하는
방법도 있다. 전자는 client가 성공을 반환하는 상태로 service가 시작되고,
후자는 build.prop에 적힌 값과 area의 값이 달라진다.

**test.** `socket_service.rs` `protocol_errors_get_init_codes`(V1 frame →
`PROP_ERROR_INVALID_CMD`), `lib.rs` `v1_set_is_refused_and_not_applied`,
`properties_service.rs` `build_prop_version_that_selects_v1_is_refused`,
`tests/v1_protocol_tests.rs` `test_v1_setprop_is_refused`.

### S10. `sys.powerctl` 저장 — 저장 동작은 (c), handler 호출 방식은 (a), 종료 뒤 거부는 (b)

**init.** `HandlePropertySet`은 이름이 `sys.powerctl`이면 값과 pid를 log에
남기고, 값이 `reboot,userspace`이면 `PROP_ERROR_INVALID_VALUE`를 보낸다
(`property_service.cpp:540-585`). `PropertySet`은 이름과 값을 검사한 뒤
`sys.powerctl`을 area에 쓰지 않는다(`:392-395`). `accept_messages`가 `true`이면
`NotifyPropertyChange`(`:178-185`)가 부르는 `PropertyChanged`가
`trigger_shutdown(value)`를 호출하고
(`init/init.cpp:380-381`), `ShutdownState::TriggerShutdown`이
`shutdown_command_`에 값을 대입한 뒤 main thread를 깨운다(`:248-260`). client에
보내는 응답은 이 대입 뒤에 나가며, main loop의 `HandlePowerctlMessage` 실행을
기다리지 않는다. `HandlePowerctlMessage`는 `shutdown[,reason]`과
`reboot[,target...]`만 처리하고, 그 밖의 값에는 `powerctl: unrecognized
command` log만 남긴다(`init/reboot.cpp`). 따라서 `PROP_SUCCESS`는 set이
수락됐다는 뜻이고, command를 인식했는지는 나타내지 않는다.

**변경 전.** 0.6.0은 `sys.powerctl`을 일반 property처럼 area에 저장했다.
`get("sys.powerctl")`이 init에서는 항상 빈 값이고 이 service에서는 마지막으로
요청한 값이었다.

**변경.** owner 결정에 따라 callback을 추가했다.
`ServiceOptions::powerctl_handler(|msg: &PowerctlMessage| -> PowerctlOutcome)`이며,
`PowerctlMessage`는 `command`(값)와 요청을 보낸 process의 `pid`, `uid`,
`gid`(SO_PEERCRED)를 담는다. 반환값 `PowerctlOutcome`은 `ShuttingDown`(host가
그 command로 shutdown 또는 reboot 절차를 시작했다)과 `Ignored`(host가 처리하지
않는 command다) 중 하나다.
- properties service의 `apply_set`이 다른 set과 같은 검사(이름, UTF-8, type,
  길이)를 하고, `reboot,userspace`는 `INVALID_VALUE`로 거부한다. 검사를
  통과하면 저장하지 않고 `PROP_SUCCESS`를 반환한다. type 검사에
  `property_info`가 필요하므로 검사 위치는 properties service다.
- socket service의 `dispatch`가 `PROP_SUCCESS`를 확인한 뒤 `notify_powerctl`을
  호출한다. peer credential을 아는 쪽이 socket service이기 때문이다.
  `notify_powerctl`은 command를 slot 하나에 기록하고 `handler_worker`를 깨운
  뒤, handler 호출을 기다리지 않고 `PROP_SUCCESS`를 반환한다. init의
  `TriggerShutdown`이 `shutdown_command_`에 값을 대입하고 main thread를 깨운 뒤
  응답하는 순서와 같다.
- slot은 하나다. `handler_worker`가 앞의 command를 꺼내기 전에 수락된 set은
  앞의 command를 덮어쓰고, handler는 나중 command로 한 번만 호출된다. init의
  `shutdown_command_`도 대입으로 덮어쓴다(`init/init.cpp:248-260`).
- `handler_worker`는 기록된 command를 다음 `ctl.*` 요청보다 먼저 꺼내 handler를
  호출한다(S3의 호출 순서).
- handler가 `ShuttingDown`을 반환하면 `handler_worker`가 `HandlerCallsStop`을
  설정한다(S3). init의 `HandlePowerctlMessage`가 command를 `shutdown` 또는
  `reboot`으로 판정한 뒤 `StopSendingMessages()`(`init/reboot.cpp:1175`)와
  `EnterShutdown()`(`:1188`)을 호출하는 지점에 해당한다. `Ignored`는 init이
  `unrecognized command` log를 남기고 반환하는 경로(`:1168-1171`)에 해당하며
  상태를 바꾸지 않는다.
- handler가 없으면 `PROP_SUCCESS`를 반환하고 `warn` log를 남긴다. init의 응답이
  command 처리 결과를 나타내지 않으므로, handler 유무로 응답을 바꾸지 않았다.
  `ctl.*`는 init의 응답이 처리 결과이므로 handler가 없으면 거부한다(S3).
- handler의 panic은 `error` log로 남기고 service는 계속 실행한다. flag는
  설정하지 않는다.

**init과 남는 차이.**
- (a) command 판정 주체. init은 `HandlePowerctlMessage`가 command를 직접
  parse한다. 이 service는 command를 해석하지 않고 handler의 반환값을 받는다.
  host의 handler가 init과 같은 판정(`shutdown[,reason]`과
  `reboot[,target...]`에만 `ShuttingDown`)을 구현해야 flag 설정 조건이 init과
  같아진다.
- (a) flag 설정 시점. init은 `HandlePowerctlMessage` 안에서, shutdown 절차를
  시작하기 전에 `StopSendingMessages()`를 호출한다. 이 service는 handler가
  반환한 뒤에 flag를 설정한다. handler 호출 중에 읽은 `ctl.*` 요청은 양쪽
  모두 queue에 들어가고 handler(control function)에 전달되지 않는다. 그
  요청의 응답은 S3의 "flag 설정 시점에 queue에 남은 요청" 항목을 따른다.
- (b) 종료 뒤 거부. `SocketService`의 `Drop` 뒤에 읽은 `sys.powerctl` 요청은
  handler를 호출하지 않고 `PROP_ERROR_SET_FAILED`를 받는다. init은
  `accept_messages`가 `false`이면 `NotifyPropertyChange`가 `PropertyChanged`를
  건너뛰고(`:178-185`) `PropertySet`은 `PROP_SUCCESS`를 반환한다(`:426-427`).
  init에서 이 상태는 유효한 command의 처리를 이미 시작한
  뒤이므로 성공 응답이 실제 상태와 어긋나지 않는다. 이 service가 종료된 뒤에는
  command를 전달받을 대상이 없으므로 실패로 응답한다.
  flag가 설정된 뒤에는(`ShuttingDown` 반환 또는
  `ServiceHandle::stop_handler_calls`) init과 같다. 검사를 통과한
  `sys.powerctl`은 handler 호출 없이 `PROP_SUCCESS`를 받는다. properties
  service가 끝난 뒤에 읽은 요청은 검사를 받지 못하므로 `PROP_ERROR_SET_FAILED`를
  받는다(S3).
- (a) 종료 대기. `ServiceHandle::shutdown`과 `join`은 `ctl.*` handler 호출과
  같은 방식으로 powerctl handler 호출이 끝날 때까지 기다린다(S3). 종료 전에
  기록된 command는 종료 뒤에도 handler에 전달된다. 따라서
  handler 안에서 `shutdown`이나 `join`을 기다리면 양쪽이 서로를 기다려 반환하지
  않는다. handler는 command를 channel로 넘기고 반환해야 한다. init의
  `TriggerShutdown`이 flag만 설정하고 main loop가 처리하는 구조와 같다.
- (b) 값 log. init은 `Received sys.powerctl='<value>' from pid`를 남긴다. 이
  service는 값을 log에 쓰지 않는 기존 정책에 따라 길이와 pid만 남긴다.
- (a) 권한 검사 없음. 다른 property와 같은 access model이다. socket에 연결할 수
  있는 process는 모두 handler를 호출할 수 있으므로, host를 reboot하는 handler는
  `uid`, `gid`, `pid`나 command를 직접 검사해야 한다.

**포기한 것.** 0.6.0에서 area의 `sys.powerctl` 값을 `wait`/`watch`로 읽던
코드는 값을 받지 못한다. handler로 옮겨야 한다. handler가 없을 때 set을
거부하는 방법도 있으나, init에서 성공하는 `setprop sys.powerctl <임의 값>`이
이 service에서 실패하게 되므로 선택하지 않았다.

**test.** `socket_service.rs` `powerctl_goes_to_the_handler_after_the_checks`,
`powerctl_keeps_one_command_and_goes_before_control`,
`shutting_down_outcome_stops_the_handler_calls`,
`close_ends_the_worker_after_its_calls`,
`stopped_handler_calls_get_init_shutdown_replies`, `lib.rs`
`powerctl_set_reaches_the_handler_and_is_not_stored`,
`powerctl_shutting_down_outcome_stops_handler_calls`,
`stop_handler_calls_answers_as_init_during_shutdown`, `properties_service.rs`
`apply_set_returns_init_codes`.

## 누락으로 보지 않는 것

- per-property SELinux / SO_PEERCRED 인가: (a)이며 `socket_service.rs`
  `SOCKET_FILE_MODE` doc의 Access model 문단에 문서화된 의도적 단순화다.
- `ContextsSplit` / `ContextsPreSplit` layout: `property_info`가 없는 이전
  Android용 layout이다. 지원 범위(API 28–37) 밖이다.
- deprecated API(`__system_property_find_nth`, `__system_property_read`,
  `__system_property_set_filename`).
- `__system_properties_zygote_reload` 자체: zygote 전용이다. 그 결과로 생기는
  override 조회는 C3에서 다룬다.
- property 파일의 root 소유 검사: bionic `prop_area.cpp:117`과 같은 조건
  (`st_uid != 0 || st_gid != 0`이면 거부)이며 divergence가 아니다. 이 crate는
  release build이거나 `strict-file-validation` feature가 켜져 있을 때 검사를
  실행한다(`file_validation.rs`). 그 결과 Linux에서 release build service는
  uid 0, gid 0으로만 시작한다. 비root로 실행하면 `publish_property_info`가
  임시 파일을 다시 읽는 단계에서 `Error::FileOwnership`이 되어
  `StartupStage::PropertiesService`로 실패한다(release build
  `example_service`를 uid 973으로 실행해 확인). 다른 소유자를 허용하는 설정은
  두지 않기로 했다. 검사가 실행되는 두 구성의 test는 CI에서 root로 실행한다.
