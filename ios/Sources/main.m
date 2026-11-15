    // iOS app shell.
//
// Two modes, picked at compile time:
//   CHATTER_RUST_UI defined  → hand off to the Rust side, which brings up
//                              Slint/winit (winit owns UIApplicationMain).
//   otherwise                → run chatter-core's smoke harness and render the
//                              report, so we can see the core working on device
//                              while the Slint iOS port is unfinished.
//
// the smoke half goes away with CHATTER_RUST_UI. Don't grow it.

#import <UIKit/UIKit.h>
#import <AVFoundation/AVFoundation.h>
#import <CoreVideo/CoreVideo.h>
#include <stdbool.h>

// Camera buffers are borrowed only for the callback. The retained session is
// owned by Rust's capture thread, and stop drains delivery before Rust drops
// its context. This session never changes the call's shared audio session.
typedef void (*ChatterCameraFrame)(void *, const uint8_t *, size_t,
                                  const uint8_t *, size_t, uint32_t, uint32_t, int64_t);
@interface ChatterCameraCapture : NSObject <AVCaptureVideoDataOutputSampleBufferDelegate>
@property(nonatomic, strong) AVCaptureSession *session;
@property(nonatomic, strong) AVCaptureVideoDataOutput *output;
@property(nonatomic, strong) dispatch_queue_t frames;
@property(nonatomic) void *context;
@property(nonatomic) ChatterCameraFrame callback;
@end

@implementation ChatterCameraCapture
- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample
       fromConnection:(AVCaptureConnection *)connection {
  CVPixelBufferRef pixel = CMSampleBufferGetImageBuffer(sample);
  if (!pixel || CVPixelBufferGetPixelFormatType(pixel) != kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ||
      CVPixelBufferGetPlaneCount(pixel) != 2 ||
      CVPixelBufferLockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly) != kCVReturnSuccess) return;
  self.callback(self.context, CVPixelBufferGetBaseAddressOfPlane(pixel, 0),
                CVPixelBufferGetBytesPerRowOfPlane(pixel, 0),
                CVPixelBufferGetBaseAddressOfPlane(pixel, 1),
                CVPixelBufferGetBytesPerRowOfPlane(pixel, 1),
                (uint32_t)CVPixelBufferGetWidth(pixel), (uint32_t)CVPixelBufferGetHeight(pixel),
                (int64_t)(CMTimeGetSeconds(CMSampleBufferGetPresentationTimeStamp(sample)) * 1000000));
  CVPixelBufferUnlockBaseAddress(pixel, kCVPixelBufferLock_ReadOnly);
}
@end

static AVCaptureDevice *chatterCameraDevice(bool front) {
  return [AVCaptureDevice defaultDeviceWithDeviceType:AVCaptureDeviceTypeBuiltInWideAngleCamera
                                          mediaType:AVMediaTypeVideo
                                           position:front ? AVCaptureDevicePositionFront : AVCaptureDevicePositionBack];
}

bool chatter_ios_camera_available(bool front) {
  @autoreleasepool { return chatterCameraDevice(front) != nil; }
}

void *chatter_ios_camera_start(bool front, uint32_t width, uint32_t fps,
                               void *context, ChatterCameraFrame callback) {
  @autoreleasepool {
    AVAuthorizationStatus permission = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
    if (permission == AVAuthorizationStatusNotDetermined) {
      // Capture runs off the UI thread. A bounded wait leaves the call usable
      // if the system permission dialog is left unanswered.
      dispatch_semaphore_t answered = dispatch_semaphore_create(0);
      [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
        dispatch_semaphore_signal(answered);
      }];
      if (dispatch_semaphore_wait(answered, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC))) return NULL;
      permission = [AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo];
    }
    if (permission != AVAuthorizationStatusAuthorized || !callback) return NULL;
    AVCaptureDevice *device = chatterCameraDevice(front);
    NSError *error = nil;
    AVCaptureDeviceInput *input = device ? [AVCaptureDeviceInput deviceInputWithDevice:device error:&error] : nil;
    if (!input) return NULL;
    ChatterCameraCapture *capture = [ChatterCameraCapture new];
    capture.context = context;
    capture.callback = callback;
    capture.frames = dispatch_queue_create("app.chatter.camera.frames", DISPATCH_QUEUE_SERIAL);
    capture.session = [AVCaptureSession new];
    capture.session.automaticallyConfiguresApplicationAudioSession = NO;
    capture.output = [AVCaptureVideoDataOutput new];
    capture.output.alwaysDiscardsLateVideoFrames = YES;
    capture.output.videoSettings = @{ (NSString *)kCVPixelBufferPixelFormatTypeKey:
                                      @(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange) };
    [capture.output setSampleBufferDelegate:capture queue:capture.frames];
    [capture.session beginConfiguration];
    NSString *preset = width <= 640 ? AVCaptureSessionPreset640x480 : AVCaptureSessionPreset1280x720;
    if (![capture.session canSetSessionPreset:preset]) {
      [capture.session commitConfiguration];
      [capture.output setSampleBufferDelegate:nil queue:NULL];
      return NULL;
    }
    capture.session.sessionPreset = preset;
    if (![capture.session canAddInput:input]) {
      [capture.session commitConfiguration];
      [capture.output setSampleBufferDelegate:nil queue:NULL];
      return NULL;
    }
    [capture.session addInput:input];
    if (![capture.session canAddOutput:capture.output]) {
      [capture.session commitConfiguration];
      [capture.output setSampleBufferDelegate:nil queue:NULL];
      return NULL;
    }
    [capture.session addOutput:capture.output];
    AVCaptureConnection *connection = [capture.output connectionWithMediaType:AVMediaTypeVideo];
    // The shared mobile UI is portrait. Rotate the actual planes so both
    // LiveKit and the local preview receive the same upright frame.
    BOOL portrait = NO;
    if (@available(iOS 17.0, *)) {
      if ([connection isVideoRotationAngleSupported:90]) {
        connection.videoRotationAngle = 90;
        portrait = YES;
      }
    } else if (connection.isVideoOrientationSupported) {
      connection.videoOrientation = AVCaptureVideoOrientationPortrait;
      portrait = YES;
    }
    [capture.session commitConfiguration];
    if (!portrait) {
      [capture.output setSampleBufferDelegate:nil queue:NULL];
      return NULL;
    }
    double targetFPS = MAX(1, MIN(fps, 30));
    for (AVFrameRateRange *range in device.activeFormat.videoSupportedFrameRateRanges) {
      if (range.minFrameRate <= targetFPS && targetFPS <= range.maxFrameRate && [device lockForConfiguration:&error]) {
        device.activeVideoMinFrameDuration = CMTimeMake(1, (int32_t)targetFPS);
        device.activeVideoMaxFrameDuration = CMTimeMake(1, (int32_t)targetFPS);
        [device unlockForConfiguration];
        break;
      }
    }
    [capture.session startRunning];
    if (!capture.session.isRunning) {
      [capture.session stopRunning];
      [capture.output setSampleBufferDelegate:nil queue:NULL];
      dispatch_sync(capture.frames, ^{});
      return NULL;
    }
    return (__bridge_retained void *)capture;
  }
}

void chatter_ios_camera_stop(void *session) {
  if (!session) return;
  @autoreleasepool {
    ChatterCameraCapture *capture = (__bridge_transfer ChatterCameraCapture *)session;
    [capture.session stopRunning];
    [capture.output setSampleBufferDelegate:nil queue:NULL];
    dispatch_sync(capture.frames, ^{});
  }
}

// Called by chatter-ui's shared clipboard adapter. Recovery codes are kept on
// this device only and expire after the same 60 seconds used on other platforms.
bool chatter_ios_clipboard_set_text(const char *utf8, bool secret) {
  if (!utf8) return false;
  NSString *text = [NSString stringWithUTF8String:utf8];
  if (!text) return false;

  void (^write)(void) = ^{
    UIPasteboard *pasteboard = UIPasteboard.generalPasteboard;
    if (secret) {
      NSDictionary *options = @{
        UIPasteboardOptionLocalOnly : @YES,
        UIPasteboardOptionExpirationDate : [NSDate dateWithTimeIntervalSinceNow:60]
      };
      [pasteboard setItems:@[ @{@"public.utf8-plain-text" : text} ] options:options];
    } else {
      pasteboard.string = text;
    }
  };
  if (NSThread.isMainThread) {
    write();
  } else {
    dispatch_sync(dispatch_get_main_queue(), write);
  }
  return true;
}

extern char *chatter_ios_smoke(const char *docs_dir);
extern void chatter_ios_string_free(char *s);

#ifdef CHATTER_RUST_UI
extern void chatter_ios_run_ui(void);
#endif

@interface AppDelegate : UIResponder <UIApplicationDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation AppDelegate

- (BOOL)application:(UIApplication *)application
    didFinishLaunchingWithOptions:(NSDictionary *)options {
  self.window = [[UIWindow alloc] initWithFrame:UIScreen.mainScreen.bounds];
  UIViewController *vc = [UIViewController new];
  vc.view.backgroundColor = UIColor.systemBackgroundColor;

  UITextView *out = [[UITextView alloc] initWithFrame:CGRectZero];
  out.editable = NO;
  out.font = [UIFont monospacedSystemFontOfSize:12 weight:UIFontWeightRegular];
  out.translatesAutoresizingMaskIntoConstraints = NO;
  [vc.view addSubview:out];
  UILayoutGuide *safe = vc.view.safeAreaLayoutGuide;
  [NSLayoutConstraint activateConstraints:@[
    [out.topAnchor constraintEqualToAnchor:safe.topAnchor constant:16],
    [out.bottomAnchor constraintEqualToAnchor:safe.bottomAnchor constant:-16],
    [out.leadingAnchor constraintEqualToAnchor:safe.leadingAnchor constant:16],
    [out.trailingAnchor constraintEqualToAnchor:safe.trailingAnchor constant:-16],
  ]];

  self.window.rootViewController = vc;
  [self.window makeKeyAndVisible];

  // Rust needs a writable dir; on iOS that's the sandbox Documents path, which
  // is far cheaper to fetch here than through a Foundation binding in Rust.
  NSString *docs = NSSearchPathForDirectoriesInDomains(NSDocumentDirectory,
                                                       NSUserDomainMask, YES)
                       .firstObject;

  // Off the main thread: the smoke run touches SQLCipher and the Keychain.
  dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
    char *report = chatter_ios_smoke(docs.UTF8String);
    NSString *text = report ? @(report) : @"chatter_ios_smoke returned NULL";
    if (report) chatter_ios_string_free(report);
    NSLog(@"[chatter] %@", text);
    dispatch_async(dispatch_get_main_queue(), ^{ out.text = text; });
  });

  return YES;
}

@end

int main(int argc, char *argv[]) {
#ifdef CHATTER_RUST_UI
  @autoreleasepool {
    // An app bundle inherits no shell environment, so the server URL rides in
    // Info.plist (CHATTERServerURL, substituted from the CHATTER_SERVER_URL
    // build setting) and is re-exported here before any Rust code reads it.
    // Bootstrap plumbing only — chatter-core still owns what to do with it.
    NSString *url = NSBundle.mainBundle.infoDictionary[@"CHATTERServerURL"];
    if (url.length) {
      setenv("CHATTER_SERVER_URL", url.UTF8String, 1);
      NSLog(@"[chatter] server url %@", url);
    }
  }
  chatter_ios_run_ui();  // never returns
  return 0;
#else
  @autoreleasepool {
    return UIApplicationMain(argc, argv, nil,
                             NSStringFromClass(AppDelegate.class));
  }
#endif
}
