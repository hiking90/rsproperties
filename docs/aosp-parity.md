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
| S2 | service | property type 검사(`CheckType`) | (c) | 이 branch에서 수정 |
| S3 | service | `ctl.*`를 일반 property로 저장 | (c) | 이 branch에서 수정 |
| S4 | service | 실패 응답 코드가 항상 `PROP_ERROR` | (c) | 이 branch에서 수정 |
| S5 | service | `ro.property_service.version` 미설정 | (c) | 이 branch에서 수정 |
| S6 | service | 변경 알림 hook 없음 | (a) | 검토 대상 아님 |
| S7 | service | properties dir writer lock(`AreaWriterLock`) | (a), 해제 방식은 (b) | lock은 0.6.0부터 있음, 해제 방식은 이 branch에서 수정 |

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
- `getprop` 예제는 인자가 없으면 toybox `getprop`처럼 이름순으로
  `[name]: [value]`를 출력한다. 열거는 `list`를 쓴다.
- 검증: API 36 emulator의 adb shell(SELinux `shell` domain)에서 이 예제의
  출력과 toybox `getprop` 출력이 531줄 모두 일치했다. Linux에서는
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
    toybox(bionic)는 `orig`를 출력한다.
  - `/dev/__properties__/appcompat_override`가 있고 파일은 355개다
    (`/dev/__properties__`는 이 directory를 포함해 356개).
  - 새 mount namespace에서 이 directory를 `/dev/__properties__` 위에 bind
    mount하면 toybox와 이 crate의 `getprop` example이 모두 override 값을
    읽는다(`debug.rsp.ac` → `ovr`, 원래 값보다 긴 `ro.*` override와 짧은
    `ro.*` override 포함). `ro.appcompat_override.debug.rsp.ac`라는 이름은 이
    namespace에서 둘 다 찾지 못한다.
  - mutable property의 override가 원래 값보다 길면 결과가 다르다.
    `debug.rsp.grow=ab`, override `abcdefgh`에서 toybox는 `abc`, 이 crate는
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
  - 같은 두 property를 설정하면 toybox와 이 crate 모두 환경변수가 없을 때
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
  읽어 toybox(`orig`)와 달랐다. 이 결과에 따라 API 37 조건을 추가했다.
  API 37 emulator(`rsbinder-a17`, android-37.0 google_apis)에서는 수정 후
  binary가 toybox와 같은 값을 읽는다(위 실측). 이 AVD는 `-memory 4096`으로
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
- 실패 상태에서는 호출마다 `property_info` open이 한 번 이상 일어난다. 첫
  실패만 `error`로, 이후는 `debug`로 log를 남긴다.
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
  않는다. init은 `foo`를 파일에 쓰고, 다음 부팅의 load가 prefix 검사에
  실패해 파일 전체를 지운다. device에서는 `property_contexts`가
  `next_boot.*`를 set할 수 있는 주체를 제한하지만, 이 service는
  per-property 인가를 하지 않는다. init처럼 파일에 썼다면 socket에 접근할 수
  있는 client는 누구나 set 한 번과 재시작 두 번으로 모든 `persist.*` 값을
  지울 수 있었다. 여기서는 그 record를 파일에 쓰지 않으므로 그 경로가 없다.
  area에 남는 값은 init과 같다. 이런 record를 area에도 적용하지 않는 방법도 있으나,
  그러면 첫 재시작 후 area 결과가 init과 달라진다.
- 손상된 파일은 owner 결정에 따라 init처럼 삭제한다.
- V1(`PROP_MSG_SETPROP`) set은 init과 같이 area에만 반영하고 파일에는 기록하지
  않는다. init의 기록 조건은
  `socket && persistent_properties_loaded && need_persist`(`:417`)이고, V1은
  `HandlePropertySetNoSocket`(socket 없음)을 거치므로 기록되지 않는다. API 36
  emulator에서 V1 frame으로 `persist.*`를 set하면 area 값은 바뀌고 파일에는
  record가 생기지 않는다. 이 service에서는 socket service가 frame의 command를
  `SetProtocol`로 넘기고, `client_set`은 `SetProtocol::V1`이면 `apply_set` 뒤에
  바로 반환한다. `PersistentStore`의 record도 고치지 않는다. init의 다음 쓰기는
  파일을 다시 읽은 내용에서 시작하므로 V1 값이 들어가지 않는데, 메모리 방식에서
  같은 결과를 내려면 record에 V1 값이 없어야 한다. V1로 set한 값을 그 뒤에 V2로
  다시 set하면 파일에 기록된다. `update`가 area가 아니라 record의 값과 비교해서
  변경 여부를 정하기 때문이다(init도 이 경우 파일에 쓴다).
  - init의 조건에 `socket &&`가 들어간 것은 `0af9ab330`(2022-10-07, "init:
    Allow persistent properties to be written asynchronously")부터다. V1
    호출부는 그 전에도 socket 없이 호출했으므로, 그 이전 init은 V1
    `persist.*`도 파일에 기록했다. commit message에는 V1에 대한 언급이 없다.
    이 service는 현재 init의 동작을 따른다.
- (b) V1 요청을 받을 때마다 `warn` level로 log를 남긴다. init은 수락한 V1 set을
  log에 남기지 않는다. socket service가 요청마다 이름, value 길이, peer pid를
  기록한다. persistent properties file이 설정된 상태에서 `persist.`/`next_boot.`
  이름이 V1로 set되면, properties service가 그 값이 area에만 있고 파일에
  기록되지 않았다는 log를 파일 경로와 함께 한 번 더 남긴다. 이 service가
  `ro.property_service.version=2`를 set하므로(S5) bionic 규칙을 따르는 client는
  V1을 보내지 않는다. V1을 보내는 client는 응답을 받지 않으므로, set이
  거부되었는지와 값이 파일에 기록되지 않았는지를 알 방법이 service의 log뿐이다.
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

type 검사는 client set과 persistent record(`init_set` → `apply_set`)에만
적용한다. build.prop entry는 검사하지 않는다(c). init `LoadProperties`는
entry마다 `CheckPermissions`를 호출하고(`property_service.cpp:803`), 그 안의
`CheckType`(`:530-533`)이 실패하면 그 entry를 버린다. `PropertyInit`은
`property_info`를 load한 뒤(`:1415`) `PropertyLoadBootDefaults`(`:1429`)를
실행하므로, 이 시점에 type 정보가 있다. 이 service의
`init_system_properties_sync`는 build.prop entry를 `add`로 바로 넣는다. 그래서
`property_contexts`가 `int`로 정한 이름에 build.prop가 `abc`를 주면, init은 그
property를 만들지 않고 이 service는 `abc`를 저장한다.

### S3. `ctl.*` 저장 — 저장 동작은 (c), dispatch 없음은 (a)

init은 `ctl.*`를 area에 저장하지 않는다. 권한을 검사한 뒤 control
message로 처리한다(`:505-512`, `:548-549`).

**변경 전(main).** build.prop loader는 `ctl.*`, `sys.powerctl`,
`selinux.restorecon_recursive`를 버린다(`build_property_parser.rs:424`).
하지만 runtime set 경로는 `ctl.start` 같은 이름을 그대로 area에 기록한다.

init은 control message를 queue에 넣고 main thread의 `HandleControlMessage`가
처리한 뒤 응답한다. 성공하면 `PROP_SUCCESS`, service가 없거나 action이
실패하면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`다(`init.cpp:556-649`).

**변경.** owner 결정에 따라 handler callback을 추가했다.
`ServiceOptions::control_handler(|msg: &ControlMessage| -> bool)`이며,
`ControlMessage`는 `action`(`ctl.` 뒤), `target`(값), `pid`(SO_PEERCRED)를
담는다.
- socket service가 이름을 검사한 뒤(`INVALID_NAME`) handler를 tokio blocking
  pool에서 호출한다. `true`면 `PROP_SUCCESS`, `false`이거나 panic이면
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`다. properties service로는 넘기지 않으므로
  area에 저장되지 않는다.
- handler가 없으면 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`로 거부한다.
- 실행 중이거나 blocking thread를 기다리는 handler 호출은 16개로 제한한다.
  permit은 `spawn_blocking` 전에 잡으므로, 17번째 요청은 blocking thread를 쓰지
  않고 `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 받는다. host runtime의
  `max_blocking_threads`가 16 이하이면 느린 호출이 pool을 채울 수 있다. 연결의 10 s
  timeout이 지나도 handler 호출은 끝까지 실행되므로, 상한이 없으면 오래
  걸리는 handler에 반복해서 보낸 `ctl.*`가 tokio blocking pool을 채운다.
  그러면 같은 pool에서 실행되는 persist 파일 쓰기가 대기하고, actor가 그
  쓰기를 await하므로 모든 set이 멈춘다. init에도 상한이 있다.
  `QueueControlMessage`(`init/init.cpp:613-623`)는 처리되지 않은 control
  message를 101개까지 queue에 두고, 그 이상은 버린다. 이때
  `SendControlMessage`(`property_service.cpp:458-463`)가 넘겨받은 fd로
  `PROP_ERROR_HANDLE_CONTROL_MESSAGE`를 보낸다. init은 queue에서 기다리는
  message 수를 세고, 이 service는 실행 중인 handler 호출 수를 센다.
- init이 하는 권한 검사(`CheckControlPropertyPerms`)는 하지 않는다. 다른
  property와 같은 access model이다.

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
- (c) `sys.powerctl`과 `selinux.restorecon_recursive`의 분기가 없다. init
  `HandlePropertySet`은 `sys.powerctl`=`reboot,userspace`에
  `PROP_ERROR_INVALID_VALUE`를 보내고(`:565-567`), `PropertySet`은
  `sys.powerctl`의 값을 area에 쓰지 않는다(`:392-395`, 처리는 property 변경
  trigger가 한다). pid가 1이 아닌 client가 `selinux.restorecon_recursive`에 빈
  값이 아닌 값을 보내면 restorecon을 시작하고 저장 없이 `PROP_SUCCESS`를
  보낸다(`:575-579`). 이 service는 두 이름 모두 일반 property처럼 저장하고
  `PROP_SUCCESS`를 보낸다. reboot과 restorecon은 Linux emulation에서 수행할
  대상이 없다(a). 다만 `get("sys.powerctl")` 결과와 `reboot,userspace` 응답
  code는 init과 다르다. build.prop loader는 두 이름을 이미 버린다
  (`build_property_parser.rs:424`).
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
응답을 기다린다. V1을 유지하려면 build.prop에
`ro.property_service.version=1`을 넣는다.
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
