#include "postgres.h"

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include "cxl_cache.h"
#include "utils/guc.h"

#define NEON_CXL_MAGIC "NEONCXL\0"
#define NEON_CXL_FORMAT_VERSION 1
#define NEON_CXL_PAGE_SIZE 8192
#define NEON_CXL_SUPERBLOCK_SIZE 4096
#define NEON_CXL_REGION_DESCRIPTOR_SIZE 64
#define NEON_CXL_REGION_HEADER_SIZE 4096
#define NEON_CXL_SLOT_META_SIZE 64
#define NEON_CXL_CONTROL_BUSY UINT64CONST(1)
#define NEON_CXL_CONTROL_VALID UINT64CONST(2)

char	   *neon_cxl_cache_file = "";
bool		neon_cxl_cache_enabled = false;

static unsigned char *cxl_mapping = NULL;
static uint64 cxl_mapping_size = 0;
static int	cxl_fd = -1;

static uint32
read_le32(const unsigned char *ptr)
{
	return ((uint32) ptr[0]) |
		((uint32) ptr[1] << 8) |
		((uint32) ptr[2] << 16) |
		((uint32) ptr[3] << 24);
}

static uint64
read_le64(const unsigned char *ptr)
{
	return ((uint64) read_le32(ptr)) |
		((uint64) read_le32(ptr + 4) << 32);
}

static uint32
neon_crc32c(const unsigned char *data, size_t length)
{
	const uint32 polynomial = 0x82f63b78U;
	uint32		crc = ~0U;

	for (size_t i = 0; i < length; i++)
	{
		crc ^= data[i];
		for (int bit = 0; bit < 8; bit++)
		{
			uint32		mask = 0U - (crc & 1U);

			crc = (crc >> 1) ^ (polynomial & mask);
		}
	}
	return ~crc;
}

static bool
ensure_mapping(char **failure_reason)
{
	struct stat statbuf;
	void	   *mapping;

	if (!neon_cxl_cache_enabled || neon_cxl_cache_file == NULL ||
		neon_cxl_cache_file[0] == '\0')
	{
		*failure_reason = "CXL cache is disabled";
		return false;
	}
	if (cxl_mapping != NULL)
		return true;

	cxl_fd = open(neon_cxl_cache_file, O_RDONLY);
	if (cxl_fd < 0)
	{
		*failure_reason = psprintf("cannot open CXL pool \"%s\": %m",
								  neon_cxl_cache_file);
		return false;
	}
	if (fstat(cxl_fd, &statbuf) != 0 || statbuf.st_size <= 0)
	{
		*failure_reason = psprintf("cannot stat CXL pool \"%s\": %m",
								  neon_cxl_cache_file);
		close(cxl_fd);
		cxl_fd = -1;
		return false;
	}
	mapping = mmap(NULL, statbuf.st_size, PROT_READ, MAP_SHARED, cxl_fd, 0);
	if (mapping == MAP_FAILED)
	{
		*failure_reason = psprintf("cannot mmap CXL pool \"%s\": %m",
								  neon_cxl_cache_file);
		close(cxl_fd);
		cxl_fd = -1;
		return false;
	}
	cxl_mapping = mapping;
	cxl_mapping_size = statbuf.st_size;
	return true;
}

bool
neon_cxl_cache_read(const NeonCxlPageLocation *location,
					char page[BLCKSZ],
					char **failure_reason)
{
	const unsigned char *header;
	const unsigned char *descriptor;
	const uint64 *control_ptr;
	const uint64 *page_words;
	uint64		control_before;
	uint64		control_after;
	uint64		descriptor_offset;
	uint64		meta_offset;
	uint64		expected_file_size;
	uint64		pool_data_size;

	*failure_reason = NULL;
	if (!ensure_mapping(failure_reason))
		return false;
	header = cxl_mapping;

	if (cxl_mapping_size < NEON_CXL_SUPERBLOCK_SIZE ||
		memcmp(header, NEON_CXL_MAGIC, 8) != 0 ||
		read_le32(header + 8) != NEON_CXL_FORMAT_VERSION)
	{
		*failure_reason = "invalid CXL pool superblock";
		return false;
	}
	if (memcmp(header + 16, location->pool_uuid, 16) != 0 ||
		read_le64(header + 32) != location->pool_epoch)
	{
		*failure_reason = "CXL pool UUID or epoch mismatch";
		return false;
	}
	pool_data_size = read_le64(header + 40);
	expected_file_size = UINT64CONST(8192) + pool_data_size;
	if (expected_file_size != cxl_mapping_size)
	{
		*failure_reason = "CXL pool file size mismatch";
		return false;
	}
	if (location->region_id >= read_le32(header + 56))
	{
		*failure_reason = "CXL region is out of range";
		return false;
	}

	descriptor_offset = NEON_CXL_SUPERBLOCK_SIZE +
		(uint64) location->region_id * NEON_CXL_REGION_DESCRIPTOR_SIZE;
	if (descriptor_offset + NEON_CXL_REGION_DESCRIPTOR_SIZE > cxl_mapping_size)
	{
		*failure_reason = "CXL region descriptor is out of bounds";
		return false;
	}
	descriptor = cxl_mapping + descriptor_offset;
	if (__atomic_load_n((const uint32 *) descriptor, __ATOMIC_ACQUIRE) != 1 ||
		read_le64(descriptor + 24) != location->region_epoch)
	{
		*failure_reason = "CXL region is not allocated or epoch is stale";
		return false;
	}
	if (location->slot_id >= read_le64(descriptor + 48))
	{
		*failure_reason = "CXL slot is out of range";
		return false;
	}
	if (location->length != NEON_CXL_PAGE_SIZE ||
		location->absolute_offset > cxl_mapping_size - NEON_CXL_PAGE_SIZE)
	{
		*failure_reason = "CXL page offset or length is invalid";
		return false;
	}

	meta_offset = read_le64(descriptor + 32) + NEON_CXL_REGION_HEADER_SIZE +
		location->slot_id * NEON_CXL_SLOT_META_SIZE;
	if (meta_offset + NEON_CXL_SLOT_META_SIZE > cxl_mapping_size)
	{
		*failure_reason = "CXL slot metadata is out of bounds";
		return false;
	}
	control_ptr = (const uint64 *) (cxl_mapping + meta_offset);
	control_before = __atomic_load_n(control_ptr, __ATOMIC_ACQUIRE);
	if (control_before != location->expected_control ||
		(control_before & NEON_CXL_CONTROL_BUSY) != 0 ||
		(control_before & NEON_CXL_CONTROL_VALID) == 0)
	{
		*failure_reason = "CXL slot sequence is stale or busy";
		return false;
	}

	page_words = (const uint64 *) (cxl_mapping + location->absolute_offset);
	for (int i = 0; i < BLCKSZ / sizeof(uint64); i++)
	{
		uint64		word = __atomic_load_n(&page_words[i], __ATOMIC_RELAXED);

		memcpy(page + i * sizeof(uint64), &word, sizeof(word));
	}
	control_after = __atomic_load_n(control_ptr, __ATOMIC_ACQUIRE);
	if (control_after != control_before)
	{
		*failure_reason = "CXL slot changed while it was being copied";
		return false;
	}
	if (neon_crc32c((const unsigned char *) page, BLCKSZ) !=
		location->checksum_crc32c)
	{
		*failure_reason = "CXL page checksum mismatch";
		return false;
	}
	return true;
}

void
neon_cxl_cache_init_gucs(void)
{
	DefineCustomBoolVariable("neon.cxl_cache_enabled",
							 "Enable the experimental file-backed CXL page cache.",
							 NULL,
							 &neon_cxl_cache_enabled,
							 false,
							 PGC_SU_BACKEND,
							 0,
							 NULL, NULL, NULL);
	DefineCustomStringVariable("neon.cxl_cache_file",
							   "Path to the file-backed CXL shared pool.",
							   NULL,
							   &neon_cxl_cache_file,
							   "",
							   PGC_SU_BACKEND,
							   0,
							   NULL, NULL, NULL);
}
